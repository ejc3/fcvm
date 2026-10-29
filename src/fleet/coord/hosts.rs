//! The GPU hosts. The coordinator launches and terminates them itself, from the host launch
//! templates: none while the fleet is asleep, `awake_capacity` per region otherwise.
//!
//! A host region short of hosts asks EC2 for the shortfall on spot first (EC2 Fleet,
//! `CreateFleet` of type `instant`, over every host template and subnet of the region, so EC2
//! picks the spot pool). The answer names the instances it launched and, for the rest, why
//! not; when spot was refused for capacity (`InsufficientInstanceCapacity`,
//! `MaxSpotInstanceCountExceeded`, ...) the same pass asks for the remainder on demand, while
//! this fleet runs fewer than `on_demand_max` on-demand hosts in all regions together. So the
//! fallback costs one more API call, not a wait. A region still short after both (no
//! capacity in either market, or the on-demand cap reached) asks again after
//! `capacity_retry`, spot first. Asleep, every host of the fleet is terminated; the next wake
//! tries spot first again.
//!
//! This replaced Auto Scaling groups whose desired capacity the coordinator set: Auto
//! Scaling took 6 to 14 s to start a launch after the desired capacity was raised, recorded a
//! refused spot launch about 10 s after that, and never moved a spot group's capacity to on
//! demand itself.
//!
//! Hosts are found by their tags (`Purpose=fcvm-fleet-host`, `Fleet=<fleet>`), so a restarted
//! coordinator keeps the hosts an earlier one launched. A host that is reclaimed (spot) or
//! stops is replaced: the regions are listed every `poll` while a wake's hosts come up and
//! every `liveness` while they run. An instance just launched counts as pending while lists
//! leave it out, for up to a minute, so DescribeInstances lagging behind CreateFleet does not
//! launch it twice; a launch whose answer is lost is asked again with the same idempotency
//! token, so EC2 answers with what it launched instead of launching more.

use super::aws::{set_items, signed, xml_tag, CredentialChain};
use super::sigv4::uri_encode;
use super::Handle;
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use fleet_core::event::ScaleInstance;
use fleet_core::Event;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;

/// A `<region>:<name>` argument (a launch template or a subnet of a region).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Regional {
    pub region: String,
    pub name: String,
}

impl std::str::FromStr for Regional {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        let (region, name) = s
            .split_once(':')
            .filter(|(r, n)| !r.is_empty() && !n.is_empty())
            .ok_or_else(|| format!("{s:?} is not <region>:<name>"))?;
        Ok(Regional {
            region: region.into(),
            name: name.into(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Market {
    Spot,
    OnDemand,
}

impl Market {
    pub fn name(self) -> &'static str {
        match self {
            Market::Spot => "spot",
            Market::OnDemand => "on-demand",
        }
    }
}

/// One GPU host instance of this fleet.
#[derive(Debug, Clone, PartialEq)]
pub struct Host {
    pub id: String,
    /// The EC2 state name (`pending`, `running`, `shutting-down`, `terminated`, `stopping`,
    /// `stopped`).
    pub state: String,
    pub market: Market,
}

impl Host {
    /// Pending or running: it is, or will be, a host.
    pub fn live(&self) -> bool {
        matches!(self.state.as_str(), "pending" | "running")
    }
    /// Not terminated and not on its way there: billed, and ours to terminate.
    pub fn present(&self) -> bool {
        !matches!(self.state.as_str(), "shutting-down" | "terminated")
    }
}

/// What a launch request got: the instances EC2 launched, and for the rest the refusals
/// for capacity (`refused`, which a spot request falls back on) and every other error
/// (`failed`).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Launch {
    pub ids: Vec<String>,
    pub refused: Vec<String>,
    pub failed: Vec<String>,
}

#[async_trait]
pub trait HostDriver: Send + Sync {
    /// This fleet's GPU hosts in `region`, in any state EC2 still lists.
    async fn hosts(&self, region: &str) -> Result<Vec<Host>>;
    /// Launch `count` hosts in `region` in `market`. `token` is the request's idempotency
    /// token: the same token with the same count answers what the first request launched
    /// instead of launching again. An error is a request that did not reach a decision (no
    /// answer, refused credentials), which may still have launched; EC2's per-instance
    /// refusals come back in [`Launch`].
    async fn launch(&self, region: &str, market: Market, count: u32, token: &str)
        -> Result<Launch>;
    async fn terminate(&self, region: &str, ids: &[String]) -> Result<()>;
}

/// The host regions, their awake capacity, and the on-demand cap.
pub struct HostConfig {
    pub driver: Arc<dyn HostDriver>,
    pub regions: Vec<String>,
    /// Hosts per region while the fleet is awake.
    pub awake_capacity: u32,
    /// At most this many on-demand hosts in all regions together.
    pub on_demand_max: u32,
    /// How often the regions are listed while hosts come up or go away.
    pub poll: Duration,
    /// How often they are listed while every host runs.
    pub liveness: Duration,
    /// How long a region that neither market could fill waits before it asks again.
    pub capacity_retry: Duration,
}

/// How long an instance a launch named counts as pending while no list names it.
const LIST_GRACE: Duration = Duration::from_secs(60);

/// How often an asleep fleet with no host left is listed (a host launched by a coordinator
/// that died mid-launch is found and terminated within this).
const ASLEEP_CHECK: Duration = Duration::from_secs(300);

/// An instance a launch named, retained through the launch visibility grace even after it runs.
struct Recent {
    region: String,
    market: Market,
    /// When the launch was asked for.
    asked: Instant,
    /// The first running observation has already been logged.
    running_reported: bool,
}

/// One region's progress through a wake.
#[derive(Debug, Default)]
struct Watch {
    /// Woken and not yet holding the awake capacity running.
    coming_up: bool,
    /// When the wake began (Unix ms), for the log.
    since: Option<u64>,
    /// No launch before this (both markets refused).
    next_launch: Option<Instant>,
}

/// The hosts of `listed` plus the instances `recent` launched in `region` that this list
/// does not name and that are younger than [`LIST_GRACE`] (as pending). A list that named an
/// instance once can omit it the next time (EC2's describe calls are eventually consistent),
/// so the grace holds whether or not an earlier list named it.
fn with_recent(
    region: &str,
    listed: Vec<Host>,
    recent: &mut HashMap<String, Recent>,
    now: Instant,
) -> Vec<Host> {
    let mut out = listed;
    let named: std::collections::HashSet<String> = out.iter().map(|h| h.id.clone()).collect();
    recent.retain(|id, r| {
        if r.region != region || named.contains(id) {
            return true;
        }
        if now.duration_since(r.asked) >= LIST_GRACE {
            return false;
        }
        out.push(Host {
            id: id.clone(),
            state: "pending".into(),
            market: r.market,
        });
        true
    });
    out
}

/// Which hosts to terminate so that `live` keeps at most `want`: on-demand before spot, and
/// pending before running.
pub fn excess(hosts: &[Host], want: u32) -> Vec<String> {
    let mut live: Vec<&Host> = hosts.iter().filter(|h| h.live()).collect();
    let n = live.len().saturating_sub(want as usize);
    live.sort_by_key(|h| (h.market == Market::Spot, h.state == "running", h.id.clone()));
    live.into_iter().take(n).map(|h| h.id.clone()).collect()
}

/// Which of a wake's running hosts came from which market.
pub fn path_name(spot: u32, on_demand: u32) -> &'static str {
    match (spot > 0, on_demand > 0) {
        (true, false) => "spot",
        (false, true) => "on-demand",
        (true, true) => "spot and on-demand",
        (false, false) => "none",
    }
}

fn lifecycle(state: &str) -> &'static str {
    match state {
        "pending" => "Pending",
        "running" => "InService",
        "stopping" => "Stopping",
        "stopped" => "Stopped",
        _ => "Terminating",
    }
}

/// Per region and market: the token and count of a launch whose outcome is unknown.
type Tokens = HashMap<(String, Market), (String, u32)>;

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Follow the fleet's power state: launch each region's shortfall (spot, then on demand in
/// the same pass), terminate what is over, report the instances while a wake's hosts come
/// up, and terminate every host while the fleet sleeps.
pub async fn run(cfg: HostConfig, mut power: watch::Receiver<String>, handle: Handle) {
    let mut watch: Vec<Watch> = cfg.regions.iter().map(|_| Watch::default()).collect();
    let mut recent: HashMap<String, Recent> = HashMap::new();
    // Per region and market, a launch whose outcome is unknown: its token and count.
    let mut tokens: Tokens = HashMap::new();
    let mut last_want: Option<u32> = None;
    // A degraded `scale` was reported and not cleared yet.
    let mut reported = false;
    loop {
        let state = power.borrow_and_update().clone();
        let want = if state == "asleep" {
            0
        } else {
            cfg.awake_capacity
        };
        let changed = last_want != Some(want);
        if changed {
            let since = unix_ms();
            for w in watch.iter_mut() {
                *w = Watch {
                    coming_up: want > 0,
                    since: (want > 0).then_some(since),
                    next_launch: None,
                };
            }
            last_want = Some(want);
            tokens.clear();
            tracing::info!(power = %state, hosts_per_region = want, "hosts set for the power state");
        }
        let now = Instant::now();
        let mut errors: Vec<String> = Vec::new();
        // Every region's hosts; `None` for a region that could not be listed.
        let mut listed: Vec<Option<Vec<Host>>> = Vec::new();
        for region in &cfg.regions {
            match cfg.driver.hosts(region).await {
                Ok(h) => listed.push(Some(with_recent(region, h, &mut recent, now))),
                Err(e) => {
                    tracing::warn!(%region, error = %format!("{e:#}"), "listing the GPU hosts failed");
                    errors.push(format!("{region}: {e:#}"));
                    listed.push(None);
                }
            }
        }
        // The fleet's on-demand hosts, when every region could be counted.
        let mut on_demand: Option<u32> = listed.iter().try_fold(0u32, |n, l| {
            l.as_ref().map(|hosts| {
                n + hosts
                    .iter()
                    .filter(|h| h.market == Market::OnDemand && h.present())
                    .count() as u32
            })
        });
        // An on-demand launch whose answer was lost may have launched: its count stays
        // reserved until a retry with its token answers.
        if let Some(n) = on_demand.as_mut() {
            *n += tokens
                .iter()
                .filter(|((_, m), _)| *m == Market::OnDemand)
                .map(|(_, (_, c))| *c)
                .sum::<u32>();
        }
        let mut report = Vec::new();
        let mut busy = false;
        for (i, region) in cfg.regions.iter().enumerate() {
            let Some(hosts) = &listed[i] else {
                busy = true;
                continue;
            };
            report.extend(hosts.iter().filter(|h| h.present()).map(|h| ScaleInstance {
                id: h.id.clone(),
                lifecycle: lifecycle(&h.state).into(),
            }));
            // Log the first running observation, retaining visibility protection through
            // the launch grace period.
            for h in hosts.iter().filter(|h| h.state == "running") {
                if let Some(r) = recent.get_mut(&h.id) {
                    if !r.running_reported {
                        tracing::info!(%region, instance = %h.id, market = h.market.name(),
                            request_to_running_ms = now.duration_since(r.asked).as_millis() as u64,
                            "wake: host running");
                        r.running_reported = true;
                    }
                }
            }
            recent.retain(|id, r| {
                r.region != *region
                    || hosts.iter().any(|h| {
                        h.id == *id
                            && (h.state == "pending"
                                || (h.state == "running"
                                    && now.duration_since(r.asked) < LIST_GRACE))
                    })
            });
            // Stopped or stopping hosts serve nothing: terminate them, as everything when
            // asleep, and what is over the awake capacity.
            let mut ids: Vec<String> = hosts
                .iter()
                .filter(|h| h.present() && (want == 0 || !h.live()))
                .map(|h| h.id.clone())
                .collect();
            if want > 0 {
                ids.extend(excess(hosts, want));
            }
            if !ids.is_empty() {
                busy = true;
                match cfg.driver.terminate(region, &ids).await {
                    Ok(()) => {
                        tracing::info!(%region, instances = ?ids, power = %state, "terminating GPU hosts")
                    }
                    Err(e) => {
                        tracing::warn!(%region, error = %format!("{e:#}"), "terminating GPU hosts failed");
                        errors.push(format!("{region}: {e:#}"));
                    }
                }
            }
            if want == 0 {
                continue;
            }
            let mut live = hosts.iter().filter(|h| h.live()).count() as u32;
            if live >= want || watch[i].next_launch.is_none_or(|t| now >= t) {
                busy |= live < want;
                // Lost launches first, each asked again with its own token and count, so EC2
                // answers with what it launched; only the shortfall left after them is new.
                if let Some(found) = reconcile(
                    &cfg,
                    region,
                    hosts,
                    &mut tokens,
                    &mut on_demand,
                    &mut recent,
                    &mut errors,
                )
                .await
                {
                    live += found;
                    busy |= found > 0;
                    if live < want {
                        let short = want - live;
                        let (launched, retry) = launch_shortfall(
                            &cfg,
                            region,
                            short,
                            &mut on_demand,
                            &mut recent,
                            &mut tokens,
                            &mut errors,
                        )
                        .await;
                        watch[i].next_launch =
                            (launched < short && retry).then(|| now + cfg.capacity_retry);
                    }
                }
            }
            let running = |m: Market| {
                hosts
                    .iter()
                    .filter(|h| h.market == m && h.state == "running")
                    .count() as u32
            };
            let (spot, od) = (running(Market::Spot), running(Market::OnDemand));
            if spot + od >= want {
                if watch[i].coming_up {
                    tracing::info!(
                        %region, path = path_name(spot, od), spot, on_demand = od,
                        since_wake_ms = watch[i].since.map(|t| unix_ms().saturating_sub(t)),
                        "wake: host capacity in service"
                    );
                }
                watch[i].coming_up = false;
            } else {
                busy = true;
            }
        }
        // The core decides whether these observations belong to an open wake.
        handle
            .send(Event::ScaleInstances {
                at: 0,
                instances: report,
            })
            .await;
        if !errors.is_empty() {
            reported = true;
            degraded(&handle, &errors.join("; ")).await;
        } else if reported || changed {
            reported = false;
            healthy(&handle).await;
        }
        let wait = if state == "waking" || busy || !errors.is_empty() {
            cfg.poll
        } else if want == 0 {
            ASLEEP_CHECK
        } else {
            cfg.liveness
        };
        tokio::select! {
            r = power.changed() => if r.is_err() { return },
            _ = tokio::time::sleep(wait) => {}
        }
    }
}

/// Ask `region`'s lost launches again with their tokens. Returns how many instances they
/// launched that `hosts` does not list yet (now followed as pending), or `None` when one
/// still has no answer (asked again at the next poll, and nothing new is launched).
async fn reconcile(
    cfg: &HostConfig,
    region: &str,
    hosts: &[Host],
    tokens: &mut Tokens,
    on_demand: &mut Option<u32>,
    recent: &mut HashMap<String, Recent>,
    errors: &mut Vec<String>,
) -> Option<u32> {
    let mut found = 0;
    for market in [Market::Spot, Market::OnDemand] {
        let key = (region.to_string(), market);
        let Some((token, count)) = tokens.get(&key).cloned() else {
            continue;
        };
        let asked = Instant::now();
        match cfg.driver.launch(region, market, count, &token).await {
            Ok(got) => {
                tokens.remove(&key);
                let new: Vec<&String> = got
                    .ids
                    .iter()
                    .filter(|id| !hosts.iter().any(|h| h.id == **id))
                    .collect();
                // A listed host can disappear from the next eventually consistent list.
                // Remember every recovered live host, not only those still unlisted.
                for id in got
                    .ids
                    .iter()
                    .filter(|id| !hosts.iter().any(|h| h.id == **id && !h.live()))
                {
                    recent.entry(id.clone()).or_insert_with(|| {
                        tracing::info!(%region, instance = %id, market = market.name(), "wake: host launch accepted (answer of a lost request)");
                        Recent {
                            region: region.to_string(),
                            market,
                            asked,
                            running_reported: false,
                        }
                    });
                }
                if market == Market::OnDemand {
                    // The reservation gives way to the hosts the list does not count yet.
                    if let Some(n) = on_demand.as_mut() {
                        *n = n.saturating_sub(count) + new.len() as u32;
                    }
                }
                found += new.len() as u32;
            }
            Err(e) => {
                tracing::warn!(%region, market = market.name(), error = %format!("{e:#}"), "asking a lost GPU host launch again failed");
                errors.push(format!("{region} {}: {e:#}", market.name()));
                return None;
            }
        }
    }
    Some(found)
}

/// Launch `short` hosts in `region`: spot, then what spot refused for capacity on demand
/// within the fleet's on-demand cap (`on_demand` counts the fleet's on-demand hosts, `None`
/// when a region could not be listed). Returns how many were launched and whether the rest
/// waits for capacity (as opposed to an error, retried at the next poll).
async fn launch_shortfall(
    cfg: &HostConfig,
    region: &str,
    short: u32,
    on_demand: &mut Option<u32>,
    recent: &mut HashMap<String, Recent>,
    tokens: &mut Tokens,
    errors: &mut Vec<String>,
) -> (u32, bool) {
    let mut launched = 0;
    let mut refused = Vec::new();
    let mut failed = false;
    for market in [Market::Spot, Market::OnDemand] {
        let ask = short - launched;
        if ask == 0 {
            break;
        }
        let ask = if market == Market::OnDemand {
            // Only what spot refused for capacity falls back, within the cap.
            if refused.is_empty() {
                break;
            }
            let Some(n) = *on_demand else {
                // Asked again at the next poll, not after the capacity retry.
                tracing::warn!(%region, "the fleet's on-demand hosts could not all be counted; not launching on demand");
                failed = true;
                break;
            };
            let room = cfg.on_demand_max.saturating_sub(n);
            if room == 0 {
                tracing::warn!(%region, on_demand = n, cap = cfg.on_demand_max, reason = %refused.join("; "),
                    "spot refused and the on-demand cap is reached; waiting for capacity");
                break;
            }
            tracing::warn!(%region, hosts = ask.min(room), reason = %refused.join("; "),
                "spot launch refused; launching on demand now");
            ask.min(room)
        } else {
            ask
        };
        let token = uuid::Uuid::new_v4().to_string();
        let asked = Instant::now();
        let got = match cfg.driver.launch(region, market, ask, &token).await {
            Ok(g) => g,
            Err(e) => {
                // It may have launched: [`reconcile`] asks again with this token, and an
                // on-demand count is reserved against the cap until then.
                tokens.insert((region.to_string(), market), (token, ask));
                if market == Market::OnDemand {
                    if let Some(n) = on_demand.as_mut() {
                        *n += ask;
                    }
                }
                tracing::warn!(%region, market = market.name(), error = %format!("{e:#}"), "launching GPU hosts failed");
                errors.push(format!("{region} {}: {e:#}", market.name()));
                return (launched, false);
            }
        };
        let request_ms = asked.elapsed().as_millis() as u64;
        for id in &got.ids {
            tracing::info!(%region, instance = %id, market = market.name(), request_ms, "wake: host launch accepted");
            recent.insert(
                id.clone(),
                Recent {
                    region: region.to_string(),
                    market,
                    asked,
                    running_reported: false,
                },
            );
        }
        if market == Market::OnDemand {
            if let Some(n) = on_demand.as_mut() {
                *n += got.ids.len() as u32;
            }
        }
        launched += got.ids.len() as u32;
        if !got.failed.is_empty() {
            let why = got.failed.join("; ");
            tracing::warn!(%region, market = market.name(), error = %why, "EC2 refused a GPU host launch");
            errors.push(format!("{region} {}: {why}", market.name()));
            failed = true;
        }
        if launched < short && got.refused.is_empty() && got.failed.is_empty() {
            failed = true;
            errors.push(format!(
                "{region} {}: launched {} of {ask} and named no error",
                market.name(),
                got.ids.len()
            ));
        }
        refused = got.refused;
    }
    if launched < short {
        tracing::warn!(%region, launched, short, "GPU hosts still short; asking again after the capacity retry");
    }
    (launched, !failed)
}

async fn degraded(handle: &Handle, reason: &str) {
    handle
        .send(Event::Degraded {
            at: 0,
            component: "scale".into(),
            reason: Some(reason.to_string()),
        })
        .await;
}

async fn healthy(handle: &Handle) {
    handle
        .send(Event::Degraded {
            at: 0,
            component: "scale".into(),
            reason: None,
        })
        .await;
}

/// Errors EC2 gives when it has no capacity (or no quota) for a market, as opposed to a
/// broken request: a spot request refused with one of these falls back to on demand.
pub const CAPACITY_CODES: &[&str] = &[
    "InsufficientInstanceCapacity",
    "InsufficientCapacity",
    "InsufficientHostCapacity",
    "UnfulfillableCapacity",
    "MaxSpotInstanceCountExceeded",
    "SpotMaxPriceTooLow",
    "VcpuLimitExceeded",
    "Unsupported",
];

/// EC2 on AWS: `CreateFleet` (type `instant`) over the region's host templates and
/// subnets, `DescribeInstances` by tag, `TerminateInstances`.
pub struct AwsHosts {
    /// The host launch templates of each region.
    pub templates: BTreeMap<String, Vec<String>>,
    /// The subnets of each region hosts may launch in.
    pub subnets: BTreeMap<String, Vec<String>>,
    /// Tagged on every host (`Fleet=<name>`); only this fleet's hosts are listed.
    pub fleet: String,
    /// Per-region API base; default `https://ec2.<region>.amazonaws.com`.
    pub endpoints: BTreeMap<String, String>,
    /// Each template's instance type, as its latest version says, and when it was read.
    types: Mutex<HashMap<(String, String), (String, Instant)>>,
    chain: CredentialChain,
    http: reqwest::Client,
}

/// How long a template's instance type is reused before it is read again.
const TEMPLATE_TYPE_TTL: Duration = Duration::from_secs(300);

impl AwsHosts {
    pub fn new(templates: &[Regional], subnets: &[Regional], fleet: String) -> Result<Self> {
        let mut t: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for r in templates {
            t.entry(r.region.clone()).or_default().push(r.name.clone());
        }
        let mut s: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for r in subnets {
            s.entry(r.region.clone()).or_default().push(r.name.clone());
        }
        for region in t.keys() {
            anyhow::ensure!(
                s.contains_key(region),
                "--host-template in {region} needs a --host-subnet in {region}"
            );
        }
        for region in s.keys() {
            anyhow::ensure!(
                t.contains_key(region),
                "--host-subnet in {region} has no --host-template in {region}"
            );
        }
        Ok(AwsHosts {
            templates: t,
            subnets: s,
            fleet,
            endpoints: BTreeMap::new(),
            types: Mutex::new(HashMap::new()),
            chain: CredentialChain::new()?,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .build()?,
        })
    }

    /// The regions with host templates, in name order.
    pub fn regions(&self) -> Vec<String> {
        self.templates.keys().cloned().collect()
    }

    fn base(&self, region: &str) -> String {
        self.endpoints
            .get(region)
            .cloned()
            .unwrap_or_else(|| format!("https://ec2.{region}.amazonaws.com"))
    }

    /// One EC2 query call; the answer's XML, or its error code and message.
    async fn call(
        &self,
        region: &str,
        params: &[(String, String)],
    ) -> Result<String, (String, String)> {
        let mut body = String::from("Version=2016-11-15");
        for (k, v) in params {
            body.push('&');
            body.push_str(&uri_encode(k));
            body.push('=');
            body.push_str(&uri_encode(v));
        }
        let (status, bytes) = signed(
            &self.chain,
            &self.http,
            &self.base(region),
            reqwest::Method::POST,
            "/",
            "",
            region,
            "ec2",
            &[(
                "content-type",
                "application/x-www-form-urlencoded; charset=utf-8".into(),
            )],
            body.into_bytes(),
        )
        .await
        .map_err(|e| (String::new(), format!("{e:#}")))?;
        let xml = String::from_utf8_lossy(&bytes).to_string();
        if status.is_success() {
            return Ok(xml);
        }
        Err((
            xml_tag(&xml, "Code").unwrap_or_default(),
            format!(
                "EC2 in {region} answered {status}: {}",
                xml_tag(&xml, "Message").unwrap_or_default()
            ),
        ))
    }

    /// The instance type of `template`'s latest version in `region`, read at most once per
    /// `TEMPLATE_TYPE_TTL`.
    async fn template_type(&self, region: &str, template: &str) -> Result<String> {
        let key = (region.to_string(), template.to_string());
        if let Some((ty, at)) = self.types.lock().unwrap().get(&key) {
            if at.elapsed() < TEMPLATE_TYPE_TTL {
                return Ok(ty.clone());
            }
        }
        let xml = self
            .call_ok(
                region,
                &[
                    p("Action", "DescribeLaunchTemplateVersions"),
                    p("LaunchTemplateName", template),
                    p("LaunchTemplateVersion.1", "$Latest"),
                ],
            )
            .await
            .with_context(|| format!("reading launch template {template} in {region}"))?;
        let ty = xml_tag(&xml, "instanceType")
            .filter(|t| !t.is_empty())
            .with_context(|| {
                format!("launch template {template} in {region} names no instance type")
            })?;
        self.types
            .lock()
            .unwrap()
            .insert(key, (ty.clone(), Instant::now()));
        Ok(ty)
    }

    async fn call_ok(&self, region: &str, params: &[(String, String)]) -> Result<String> {
        self.call(region, params)
            .await
            .map_err(|(code, msg)| anyhow::anyhow!("{code}: {msg}"))
    }
}

fn p(k: impl Into<String>, v: impl Into<String>) -> (String, String) {
    (k.into(), v.into())
}

/// The `CreateFleet` (instant) parameters for `count` hosts in `market` over `templates` x
/// `subnets`, tagged for `fleet`. `templates` pairs each template name with its instance
/// type, which every override names: IAM checks `ec2:InstanceType` on the fleet's instances,
/// and for `CreateFleet` it takes that key only from the request, not from the template.
pub fn create_fleet_params(
    templates: &[(String, String)],
    subnets: &[String],
    market: Market,
    count: u32,
    fleet: &str,
    token: &str,
) -> Vec<(String, String)> {
    let mut v = vec![
        p("Action", "CreateFleet"),
        p("Type", "instant"),
        p("ClientToken", token),
        p(
            "TargetCapacitySpecification.TotalTargetCapacity",
            count.to_string(),
        ),
        p(
            "TargetCapacitySpecification.DefaultTargetCapacityType",
            market.name(),
        ),
    ];
    match market {
        Market::Spot => {
            v.push(p(
                "TargetCapacitySpecification.SpotTargetCapacity",
                count.to_string(),
            ));
            v.push(p(
                "SpotOptions.AllocationStrategy",
                "price-capacity-optimized",
            ));
            v.push(p("SpotOptions.InstanceInterruptionBehavior", "terminate"));
        }
        Market::OnDemand => {
            v.push(p(
                "TargetCapacitySpecification.OnDemandTargetCapacity",
                count.to_string(),
            ));
        }
    }
    for (i, (t, ty)) in templates.iter().enumerate() {
        let c = format!("LaunchTemplateConfigs.{}", i + 1);
        v.push(p(
            format!("{c}.LaunchTemplateSpecification.LaunchTemplateName"),
            t.as_str(),
        ));
        v.push(p(
            format!("{c}.LaunchTemplateSpecification.Version"),
            "$Latest",
        ));
        for (j, s) in subnets.iter().enumerate() {
            v.push(p(format!("{c}.Overrides.{}.SubnetId", j + 1), s.as_str()));
            v.push(p(
                format!("{c}.Overrides.{}.InstanceType", j + 1),
                ty.as_str(),
            ));
        }
    }
    for (i, (k, val)) in [
        ("Name", "fcvm-fleet-host"),
        ("Purpose", "fcvm-fleet-host"),
        ("Fleet", fleet),
        ("Market", market.name()),
    ]
    .into_iter()
    .enumerate()
    {
        let n = i + 1;
        if n == 1 {
            v.push(p("TagSpecification.1.ResourceType", "instance"));
        }
        v.push(p(format!("TagSpecification.1.Tag.{n}.Key"), k));
        v.push(p(format!("TagSpecification.1.Tag.{n}.Value"), val));
    }
    v
}

/// Every `<item>` value of every `<name>` list of `xml`.
fn items_of(xml: &str, name: &str) -> Vec<String> {
    let (open, close) = (format!("<{name}>"), format!("</{name}>"));
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find(&open) {
        let after = &rest[i + open.len()..];
        let Some(end) = after.find(&close) else { break };
        let mut inner = &after[..end];
        while let Some(j) = inner.find("<item>") {
            let a = &inner[j + "<item>".len()..];
            let Some(k) = a.find("</item>") else { break };
            out.push(a[..k].trim().to_string());
            inner = &a[k..];
        }
        rest = &after[end..];
    }
    out
}

/// A `CreateFleet` (instant) answer: the instances launched, and its errors split into
/// capacity refusals and everything else, each `<code>: <message>`, without repeats.
pub fn parse_create_fleet(xml: &str) -> Launch {
    let mut out = Launch {
        ids: items_of(xml, "instanceIds"),
        ..Launch::default()
    };
    for item in set_items(xml, "errorSet") {
        let Some(code) = xml_tag(item, "errorCode") else {
            continue;
        };
        let msg = xml_tag(item, "errorMessage").unwrap_or_default();
        let line = format!("{code}: {msg}");
        let list = if CAPACITY_CODES.contains(&code.as_str()) {
            &mut out.refused
        } else {
            &mut out.failed
        };
        if !list.contains(&line) {
            list.push(line);
        }
    }
    out
}

/// The instances of a DescribeInstances answer, one per item of its `instancesSet` lists
/// (EC2 does not promise the order of an item's elements, so the item bounds an instance),
/// with their state and market (an instance with `<instanceLifecycle>spot` is spot; one
/// without is on demand).
pub fn parse_hosts(xml: &str) -> Vec<Host> {
    set_items(xml, "instancesSet")
        .into_iter()
        .filter_map(|seg| {
            Some(Host {
                id: xml_tag(seg, "instanceId")?,
                state: xml_tag(seg, "instanceState")
                    .and_then(|s| xml_tag(&s, "name"))
                    .unwrap_or_default(),
                market: if xml_tag(seg, "instanceLifecycle").as_deref() == Some("spot") {
                    Market::Spot
                } else {
                    Market::OnDemand
                },
            })
        })
        .collect()
}

#[async_trait]
impl HostDriver for AwsHosts {
    async fn hosts(&self, region: &str) -> Result<Vec<Host>> {
        let mut out: Vec<Host> = Vec::new();
        let mut next: Option<String> = None;
        loop {
            let mut params = vec![
                p("Action", "DescribeInstances"),
                p("Filter.1.Name", "tag:Purpose"),
                p("Filter.1.Value.1", "fcvm-fleet-host"),
                p("Filter.2.Name", "tag:Fleet"),
                p("Filter.2.Value.1", self.fleet.as_str()),
            ];
            if let Some(t) = &next {
                params.push(p("NextToken", t.as_str()));
            }
            let xml = self.call_ok(region, &params).await?;
            out.extend(parse_hosts(&xml));
            next = xml_tag(&xml, "nextToken").filter(|t| !t.is_empty());
            if next.is_none() {
                break;
            }
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out.dedup_by(|a, b| a.id == b.id);
        Ok(out)
    }

    async fn launch(
        &self,
        region: &str,
        market: Market,
        count: u32,
        token: &str,
    ) -> Result<Launch> {
        let templates = self
            .templates
            .get(region)
            .with_context(|| format!("no host template in {region}"))?;
        let subnets = self
            .subnets
            .get(region)
            .with_context(|| format!("no host subnet in {region}"))?;
        let mut typed = Vec::with_capacity(templates.len());
        for t in templates {
            typed.push((t.clone(), self.template_type(region, t).await?));
        }
        let params = create_fleet_params(&typed, subnets, market, count, &self.fleet, token);
        match self.call(region, &params).await {
            Ok(xml) => Ok(parse_create_fleet(&xml)),
            // A request refused as a whole for capacity is a refusal like a per-pool one.
            Err((code, msg)) if CAPACITY_CODES.contains(&code.as_str()) => Ok(Launch {
                refused: vec![format!("{code}: {msg}")],
                ..Launch::default()
            }),
            Err((code, msg)) => bail!("CreateFleet: {code}: {msg}"),
        }
    }

    async fn terminate(&self, region: &str, ids: &[String]) -> Result<()> {
        let mut params = vec![p("Action", "TerminateInstances")];
        for (i, id) in ids.iter().enumerate() {
            params.push(p(format!("InstanceId.{}", i + 1), id.as_str()));
        }
        self.call_ok(region, &params).await.map(|_| ())
    }
}

/// Hosts in memory. Each region's spot and on-demand capacity is set by the test (`None`:
/// unlimited); a launch beyond it is refused with `InsufficientInstanceCapacity`. Launched
/// hosts start in `launch_state` (default `pending`); `set_state` moves them. Every call is
/// recorded in `calls`, in order: `list <region>`, `launch <region> <market> <count>`,
/// `terminate <region> <ids>`. `fail_lists` makes that many next lists of a region fail.
/// A launch is idempotent by its token. `ambiguous` makes that many next launches that launch
/// something fail after launching, as a request whose answer is lost. `hide_new` (show, hide): each launched
/// instance is named by its region's next `show` lists, left out of the `hide` after them,
/// then named again, as an eventually consistent DescribeInstances.
#[derive(Default)]
pub struct FakeHosts {
    pub hosts: Mutex<BTreeMap<String, Vec<Host>>>,
    pub capacity: Mutex<HashMap<(String, Market), u32>>,
    pub launch_state: Mutex<Option<String>>,
    pub calls: Mutex<Vec<String>>,
    pub fail_lists: Mutex<HashMap<String, u32>>,
    pub ambiguous: Mutex<u32>,
    pub hide_new: Mutex<Option<(u32, u32)>>,
    by_token: Mutex<HashMap<String, Launch>>,
    visibility: Mutex<HashMap<String, (u32, u32)>>,
    next: std::sync::atomic::AtomicU64,
}

impl FakeHosts {
    /// Every host of `region` in a present state (not shutting down or terminated).
    pub fn present(&self, region: &str) -> Vec<Host> {
        self.hosts
            .lock()
            .unwrap()
            .get(region)
            .map(|v| v.iter().filter(|h| h.present()).cloned().collect())
            .unwrap_or_default()
    }

    /// Move every host of `region` in `from` to `to`.
    pub fn set_state(&self, region: &str, from: &str, to: &str) {
        if let Some(v) = self.hosts.lock().unwrap().get_mut(region) {
            for h in v.iter_mut().filter(|h| h.state == from) {
                h.state = to.into();
            }
        }
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl HostDriver for FakeHosts {
    async fn hosts(&self, region: &str) -> Result<Vec<Host>> {
        self.calls.lock().unwrap().push(format!("list {region}"));
        if let Some(n) = self.fail_lists.lock().unwrap().get_mut(region) {
            if *n > 0 {
                *n -= 1;
                bail!("injected list failure in {region}");
            }
        }
        let all = self
            .hosts
            .lock()
            .unwrap()
            .get(region)
            .cloned()
            .unwrap_or_default();
        let mut vis = self.visibility.lock().unwrap();
        Ok(all
            .into_iter()
            .filter(|h| match vis.get_mut(&h.id) {
                Some((show, _)) if *show > 0 => {
                    *show -= 1;
                    true
                }
                Some((_, hide)) if *hide > 0 => {
                    *hide -= 1;
                    false
                }
                _ => true,
            })
            .collect())
    }

    async fn launch(
        &self,
        region: &str,
        market: Market,
        count: u32,
        token: &str,
    ) -> Result<Launch> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("launch {region} {} {count}", market.name()));
        if let Some(l) = self.by_token.lock().unwrap().get(token) {
            return Ok(l.clone());
        }
        let n = {
            let mut cap = self.capacity.lock().unwrap();
            match cap.get_mut(&(region.to_string(), market)) {
                Some(c) => {
                    let n = count.min(*c);
                    *c -= n;
                    n
                }
                None => count,
            }
        };
        let state = self
            .launch_state
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| "pending".into());
        let mut out = Launch::default();
        for _ in 0..n {
            let id = format!(
                "i-{:017x}",
                self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            );
            self.hosts
                .lock()
                .unwrap()
                .entry(region.to_string())
                .or_default()
                .push(Host {
                    id: id.clone(),
                    state: state.clone(),
                    market,
                });
            if let Some(v) = *self.hide_new.lock().unwrap() {
                self.visibility.lock().unwrap().insert(id.clone(), v);
            }
            out.ids.push(id);
        }
        if n < count {
            out.refused.push(format!(
                "InsufficientInstanceCapacity: no {} capacity",
                market.name()
            ));
        }
        self.by_token
            .lock()
            .unwrap()
            .insert(token.to_string(), out.clone());
        let mut amb = self.ambiguous.lock().unwrap();
        if *amb > 0 && n > 0 {
            *amb -= 1;
            bail!("injected: the launch's answer was lost");
        }
        Ok(out)
    }

    async fn terminate(&self, region: &str, ids: &[String]) -> Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("terminate {region} {}", ids.join(",")));
        if let Some(v) = self.hosts.lock().unwrap().get_mut(region) {
            for h in v.iter_mut().filter(|h| ids.contains(&h.id)) {
                h.state = "shutting-down".into();
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(id: &str, state: &str, market: Market) -> Host {
        Host {
            id: id.into(),
            state: state.into(),
            market,
        }
    }

    #[test]
    fn regional_arguments_parse() {
        let r: Regional = "us-east-1:fcvm-fleet-host-x86_64".parse().unwrap();
        assert_eq!(r.region, "us-east-1");
        assert_eq!(r.name, "fcvm-fleet-host-x86_64");
        assert!("fcvm-fleet-host-x86_64".parse::<Regional>().is_err());
        assert!(":x".parse::<Regional>().is_err());
    }

    /// The request names every template with every subnet as an override, asks for the
    /// market's capacity, and tags the instances with the fleet and the market.
    #[test]
    fn create_fleet_asks_for_the_market_over_every_template_and_subnet() {
        let got = create_fleet_params(
            &[
                ("tpl-a".into(), "g4dn.metal".into()),
                ("tpl-b".into(), "g5g.metal".into()),
            ],
            &["subnet-1".into(), "subnet-2".into()],
            Market::Spot,
            2,
            "stage",
            "token",
        );
        let get = |k: &str| {
            got.iter()
                .find(|(a, _)| a == k)
                .map(|(_, v)| v.as_str())
                .unwrap_or_else(|| panic!("no {k} in {got:?}"))
        };
        assert_eq!(get("Type"), "instant");
        assert_eq!(get("TargetCapacitySpecification.TotalTargetCapacity"), "2");
        assert_eq!(get("TargetCapacitySpecification.SpotTargetCapacity"), "2");
        assert_eq!(
            get("TargetCapacitySpecification.DefaultTargetCapacityType"),
            "spot"
        );
        assert_eq!(
            get("SpotOptions.AllocationStrategy"),
            "price-capacity-optimized"
        );
        assert_eq!(
            get("LaunchTemplateConfigs.2.LaunchTemplateSpecification.LaunchTemplateName"),
            "tpl-b"
        );
        assert_eq!(
            get("LaunchTemplateConfigs.2.Overrides.2.SubnetId"),
            "subnet-2"
        );
        assert_eq!(
            get("LaunchTemplateConfigs.1.Overrides.2.InstanceType"),
            "g4dn.metal"
        );
        assert_eq!(
            get("LaunchTemplateConfigs.2.Overrides.1.InstanceType"),
            "g5g.metal"
        );
        assert_eq!(get("TagSpecification.1.ResourceType"), "instance");
        assert_eq!(get("TagSpecification.1.Tag.3.Key"), "Fleet");
        assert_eq!(get("TagSpecification.1.Tag.3.Value"), "stage");
        let od = create_fleet_params(
            &[("t".into(), "g4dn.metal".into())],
            &["s".into()],
            Market::OnDemand,
            1,
            "stage",
            "x",
        );
        assert!(od
            .iter()
            .any(|(k, v)| k == "TargetCapacitySpecification.OnDemandTargetCapacity" && v == "1"));
        assert!(!od.iter().any(|(k, _)| k.starts_with("SpotOptions")));
        assert!(od.iter().any(|(k, v)| k
            == "TargetCapacitySpecification.DefaultTargetCapacityType"
            && v == "on-demand"));
    }

    /// An instant fleet's answer, in the shape the EC2 API reference gives for CreateFleet
    /// (ids replaced): one instance launched, two pools refused for capacity, one error
    /// that is not capacity.
    #[test]
    fn create_fleet_answers_parse_to_instances_and_refusals() {
        let xml = r#"<CreateFleetResponse xmlns="http://ec2.amazonaws.com/doc/2016-11-15/">
  <requestId>r-0</requestId><fleetId>fleet-0123456789abcdef0</fleetId>
  <errorSet>
    <item><launchTemplateAndOverrides><overrides><subnetId>subnet-1</subnetId></overrides></launchTemplateAndOverrides>
      <lifecycle>spot</lifecycle><errorCode>InsufficientInstanceCapacity</errorCode>
      <errorMessage>There is no Spot capacity available that matches your request.</errorMessage></item>
    <item><lifecycle>spot</lifecycle><errorCode>InsufficientInstanceCapacity</errorCode>
      <errorMessage>There is no Spot capacity available that matches your request.</errorMessage></item>
    <item><errorCode>InvalidLaunchTemplateName.NotFoundException</errorCode><errorMessage>no such template</errorMessage></item>
  </errorSet>
  <fleetInstanceSet>
    <item><launchTemplateAndOverrides><overrides><subnetId>subnet-2</subnetId></overrides></launchTemplateAndOverrides>
      <lifecycle>spot</lifecycle><instanceIds><item>i-0123456789abcdef0</item></instanceIds>
      <instanceType>g4dn.metal</instanceType></item>
  </fleetInstanceSet>
</CreateFleetResponse>"#;
        let got = parse_create_fleet(xml);
        assert_eq!(got.ids, vec!["i-0123456789abcdef0".to_string()]);
        assert_eq!(got.refused.len(), 1, "{got:?}");
        assert!(got.refused[0].starts_with("InsufficientInstanceCapacity: "));
        assert_eq!(got.failed.len(), 1, "{got:?}");
        let none =
            parse_create_fleet("<CreateFleetResponse><fleetInstanceSet/></CreateFleetResponse>");
        assert_eq!(none, Launch::default());
    }

    #[test]
    fn describe_answers_parse_to_hosts_with_their_market() {
        let xml = r#"<DescribeInstancesResponse><reservationSet><item><instancesSet>
  <item><instanceId>i-0123456789abcdef0</instanceId><instanceState><code>16</code><name>running</name></instanceState>
    <instanceLifecycle>spot</instanceLifecycle><tagSet><item><key>Purpose</key><value>fcvm-fleet-host</value></item></tagSet></item>
  <item><instanceId>i-1234567890abcdef0</instanceId><instanceState><code>0</code><name>pending</name></instanceState></item>
</instancesSet></item></reservationSet></DescribeInstancesResponse>"#;
        assert_eq!(
            parse_hosts(xml),
            vec![
                host("i-0123456789abcdef0", "running", Market::Spot),
                host("i-1234567890abcdef0", "pending", Market::OnDemand),
            ]
        );
    }

    /// EC2 does not promise the order of an instance's elements: a state or market before
    /// `<instanceId>` belongs to that instance, not to the one listed before it.
    #[test]
    fn describe_answers_parse_by_item_whatever_the_field_order() {
        let xml = r#"<DescribeInstancesResponse><reservationSet><item><instancesSet>
  <item><instanceState><code>16</code><name>running</name></instanceState><instanceLifecycle>spot</instanceLifecycle>
    <tagSet><item><key>Purpose</key><value>fcvm-fleet-host</value></item></tagSet><instanceId>i-0123456789abcdef0</instanceId></item>
  <item><instanceId>i-1234567890abcdef0</instanceId><instanceState><code>0</code><name>pending</name></instanceState></item>
</instancesSet></item></reservationSet></DescribeInstancesResponse>"#;
        assert_eq!(
            parse_hosts(xml),
            vec![
                host("i-0123456789abcdef0", "running", Market::Spot),
                host("i-1234567890abcdef0", "pending", Market::OnDemand),
            ]
        );
    }

    #[test]
    fn the_excess_goes_on_demand_first_then_pending() {
        let hosts = [
            host("i-1", "running", Market::Spot),
            host("i-2", "pending", Market::Spot),
            host("i-3", "running", Market::OnDemand),
            host("i-4", "terminated", Market::OnDemand),
        ];
        assert_eq!(
            excess(&hosts, 1),
            vec!["i-3".to_string(), "i-2".to_string()]
        );
        assert!(excess(&hosts, 3).is_empty());
    }

    #[test]
    fn a_launched_instance_counts_as_pending_until_a_list_names_it() {
        let now = Instant::now();
        let mut recent = HashMap::new();
        recent.insert(
            "i-new".to_string(),
            Recent {
                region: "us-east-1".into(),
                market: Market::Spot,
                asked: now,
                running_reported: false,
            },
        );
        let got = with_recent("us-east-1", vec![], &mut recent, now);
        assert_eq!(got, vec![host("i-new", "pending", Market::Spot)]);
        // Another region does not see it.
        assert!(with_recent("us-east-2", vec![], &mut recent, now).is_empty());
        // Listed: the list's state stands, nothing is added.
        let got = with_recent(
            "us-east-1",
            vec![host("i-new", "running", Market::Spot)],
            &mut recent,
            now,
        );
        assert_eq!(got, vec![host("i-new", "running", Market::Spot)]);
        // Named once, then left out of a list within the grace: still pending.
        let got = with_recent("us-east-1", vec![], &mut recent, now);
        assert_eq!(got, vec![host("i-new", "pending", Market::Spot)]);
        // Never listed and past the grace: dropped.
        let mut stale = HashMap::new();
        stale.insert(
            "i-old".to_string(),
            Recent {
                region: "us-east-1".into(),
                market: Market::Spot,
                asked: now,
                running_reported: false,
            },
        );
        assert!(with_recent("us-east-1", vec![], &mut stale, now + LIST_GRACE).is_empty());
        assert!(stale.is_empty());
    }

    #[test]
    fn the_path_names_the_markets_the_hosts_came_from() {
        assert_eq!(path_name(1, 0), "spot");
        assert_eq!(path_name(0, 1), "on-demand");
        assert_eq!(path_name(1, 1), "spot and on-demand");
    }
}
