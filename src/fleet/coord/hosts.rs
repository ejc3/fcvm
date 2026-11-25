//! The GPU hosts, and pages mode's host. The coordinator launches and terminates them
//! itself, from the launch templates of each host group ([`HostConfig`]): none while the
//! fleet is asleep, `awake_hosts` per region otherwise. Each group runs its own [`run`] over
//! the same power state, and lists, launches and terminates only its own class's instances
//! ([`HostClass`]), so its on-demand count and cap and its degraded component are its own.
//! Pages mode's host group is one host in one region, with an on-demand cap of one; it
//! reports nothing a wake's record or the wall's tiling counts GPU hosts by.
//!
//! A host region short of hosts asks EC2 for the shortfall on spot first (EC2 Fleet,
//! `CreateFleet` of type `instant`, over every host template and subnet of the region, so EC2
//! picks the spot pool). The answer names the instances it launched and, for the rest, why
//! not; when spot was refused for capacity (`InsufficientInstanceCapacity`,
//! `MaxSpotInstanceCountExceeded`, ...) the same pass asks for the remainder on demand, while
//! this fleet runs fewer than `on_demand_max` on-demand hosts in all regions together. So the
//! fallback costs one more API call, not a wait. A region still short after both (no
//! capacity in either market, or the on-demand cap reached) asks again after
//! `capacity_retry`, spot first. A region left short by the account's quota
//! (`VcpuLimitExceeded`, `MaxSpotInstanceCountExceeded`) also marks the coordinator degraded
//! (`scale` for the GPU hosts, `scale-renders` for pages mode's host). Asleep, every host of
//! the fleet is terminated; the next wake tries spot first again.
//!
//! This replaced Auto Scaling groups whose desired capacity the coordinator set: Auto
//! Scaling took 6 to 14 s to start a launch after the desired capacity was raised, recorded a
//! refused spot launch about 10 s after that, and never moved a spot group's capacity to on
//! demand itself.
//!
//! Hosts are found by their tags (`Purpose=fcvm-fleet-host`, or `fcvm-fleet-renders` for
//! pages mode's host, and `Fleet=<fleet>`), so a restarted coordinator keeps the hosts an
//! earlier one launched. A host that is reclaimed (spot) or
//! stops is replaced: the regions are listed every `poll` while a wake's hosts come up and
//! every `liveness` while they run. An instance just launched counts as pending while lists
//! leave it out, for up to a minute, so DescribeInstances lagging behind CreateFleet does not
//! launch it twice; a launch whose answer is lost is asked again with the same idempotency
//! token, so EC2 answers with what it launched instead of launching more.
//!
//! With more than one awake host per region, a host whose agent reported a spot interruption
//! notice (`draining`, from the state) is not counted: its replacement is launched at the
//! notice, not when EC2 takes the instance two minutes later, and the draining host is
//! terminated once the region again runs its awake hosts without it. Asleep, it is terminated
//! with the rest. With one, the replacement waits for EC2 to take the instance, as the
//! region's spot quota usually holds one host.
//!
//! With `--host-spread az` each host is launched on its own, into the subnets of availability
//! zones no live host of the region is in (a draining host's zone included), so one zone's
//! spot reclaim or capacity shortage takes at most one host. When every zone holds a host,
//! the zones of draining hosts are allowed again, then every zone; a tier that both markets
//! refused for capacity falls through to the next in the same pass, since a second host in a
//! used zone shows more of the wall than none.

use super::aws::{set_items, signed, xml_tag, CredentialChain};
use super::sigv4::uri_encode;
use super::Handle;
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use fleet_core::event::ScaleInstance;
use fleet_core::Event;
use std::collections::{BTreeMap, BTreeSet, HashMap};
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

/// How a region's hosts are placed across its availability zones (`--host-spread`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum HostSpread {
    /// One request for the region's whole shortfall; EC2 picks the subnets.
    #[default]
    None,
    /// One host per request, into a zone no live host of the region is in when the region
    /// has a subnet in one.
    Az,
}

/// A host group's class: what its instances are for. Each group lists, launches and
/// terminates only the instances tagged with its class's `Purpose`, so its counts (awake,
/// on demand, excess) are its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HostClass {
    /// The GPU hosts of the wall and the GPU modes (`--host-template`).
    Gpu,
    /// The CPU host that runs pages mode alone (`--renders-template`): no GPU, no place on
    /// the wall and no part in a wake's record.
    Renders,
}

impl HostClass {
    /// The `Purpose` tag of the class's instances.
    pub fn purpose(self) -> &'static str {
        match self {
            HostClass::Gpu => "fcvm-fleet-host",
            HostClass::Renders => "fcvm-fleet-renders",
        }
    }

    /// The coordinator component a quota or launch failure of the class marks degraded.
    pub fn component(self) -> &'static str {
        match self {
            HostClass::Gpu => "scale",
            HostClass::Renders => "scale-renders",
        }
    }

    /// The class's hosts, as the log names them.
    pub fn noun(self) -> &'static str {
        match self {
            HostClass::Gpu => "GPU host",
            HostClass::Renders => "renders host",
        }
    }

    /// The class's launch template and subnet flags, for errors.
    fn flags(self) -> (&'static str, &'static str) {
        match self {
            HostClass::Gpu => ("--host-template", "--host-subnet"),
            HostClass::Renders => ("--renders-template", "--renders-subnet"),
        }
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

/// One host instance of this fleet.
#[derive(Debug, Clone, PartialEq)]
pub struct Host {
    pub id: String,
    /// The EC2 state name (`pending`, `running`, `shutting-down`, `terminated`, `stopping`,
    /// `stopped`).
    pub state: String,
    pub market: Market,
    /// Where a connection from this instance can come from: its public IPv4 address, its IPv6
    /// addresses, and its delegated IPv6 prefixes as `<address>/<length>`.
    pub sources: Vec<String>,
    /// Its availability zone, when known.
    pub zone: Option<String>,
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
    /// The availability zone of each launched instance the answer places.
    pub zones: BTreeMap<String, String>,
}

#[async_trait]
pub trait HostDriver: Send + Sync {
    /// This fleet's hosts of the driver's class in `region`, in any state EC2 still lists.
    async fn hosts(&self, region: &str) -> Result<Vec<Host>>;
    /// The availability zones of `region`'s host subnets, each once.
    async fn zones(&self, region: &str) -> Result<Vec<String>>;
    /// Launch `count` hosts in `region` in `market`, in the subnets of `zones` (every subnet
    /// when empty). `token` is the request's idempotency token: the same token with the same
    /// count answers what the first request launched instead of launching again. An error is a
    /// request that did not reach a decision (no answer, refused credentials), which may still
    /// have launched; EC2's per-instance refusals come back in [`Launch`].
    async fn launch(
        &self,
        region: &str,
        market: Market,
        count: u32,
        token: &str,
        zones: &[String],
    ) -> Result<Launch>;
    async fn terminate(&self, region: &str, ids: &[String]) -> Result<()>;
}

/// The host regions, their awake capacity, and the on-demand cap.
pub struct HostConfig {
    /// What the group's hosts are for.
    pub class: HostClass,
    pub driver: Arc<dyn HostDriver>,
    pub regions: Vec<String>,
    /// Hosts per region while the fleet is awake.
    pub awake_hosts: u32,
    /// How a region's hosts are placed across its availability zones.
    pub spread: HostSpread,
    /// At most this many on-demand hosts in all regions together.
    pub on_demand_max: u32,
    /// How often the regions are listed while hosts come up or go away.
    pub poll: Duration,
    /// How often they are listed while every host runs.
    pub liveness: Duration,
    /// How long a region that neither market could fill waits before it asks again.
    pub capacity_retry: Duration,
    /// How long a lost launch's token is asked again. By then a list names whatever it
    /// launched (`LIST_GRACE`), so dropping it launches nothing twice, and a token whose
    /// retry never gets an answer stops holding its region's launches.
    pub lost_launch_ttl: Duration,
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
    /// Its availability zone, when the launch's answer placed it.
    zone: Option<String>,
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
    /// The account's quota refused the last launch attempt: reported as degraded on every
    /// pass, including the polls between attempts, until an attempt is refused for something
    /// else or the region holds its awake hosts.
    quota: Option<String>,
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
            sources: Vec::new(),
            zone: r.zone.clone(),
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

/// The hosts of `hosts` that serve: pending or running, and not draining.
pub fn serving(hosts: &[Host], draining: &BTreeSet<String>) -> Vec<Host> {
    hosts
        .iter()
        .filter(|h| h.live() && !draining.contains(&h.id))
        .cloned()
        .collect()
}

/// The hosts the launcher keeps for `want` per region, by instance id, sorted: every
/// region's serving hosts (pending or running, not draining) less those over the capacity.
/// `None` while any region is unlisted or short of `want`, or asleep.
pub fn expected_hosts(
    listed: &[Option<Vec<Host>>],
    want: u32,
    draining: &BTreeSet<String>,
) -> Option<Vec<String>> {
    if want == 0 {
        return None;
    }
    let mut ids = Vec::new();
    for hosts in listed {
        let hosts = serving(hosts.as_ref()?, draining);
        let hosts = &hosts[..];
        let over = excess(hosts, want);
        let kept: Vec<String> = hosts
            .iter()
            .filter(|h| h.live() && !over.contains(&h.id))
            .map(|h| h.id.clone())
            .collect();
        if kept.len() < want as usize {
            return None;
        }
        ids.extend(kept);
    }
    ids.sort();
    Some(ids)
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

/// Per region and market: the token and count of a launch whose outcome is unknown, when it
/// was first asked, and when it was last asked.
type Tokens = HashMap<(String, Market), (String, u32, Instant, Instant)>;

/// What a lost launch's token does in a pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenStep {
    /// Asked again with its token.
    Ask,
    /// Not asked any more, but its region still launches nothing new and its on-demand count
    /// stays reserved: its last ask may have launched a host no list names yet.
    Hold,
    /// Forgotten: every ask was long enough ago for a list to name what it launched.
    Drop,
}

/// A token is asked again while younger than `ttl`, then held until `ttl` has passed since its
/// last ask as well (`ttl` is longer than `LIST_GRACE`), then dropped. A retry that is never
/// answered so holds its region for at most twice `ttl`.
fn token_step(first: Instant, last: Instant, now: Instant, ttl: Duration) -> TokenStep {
    if now.duration_since(first) < ttl {
        TokenStep::Ask
    } else if now.duration_since(last) < ttl {
        TokenStep::Hold
    } else {
        TokenStep::Drop
    }
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Follow the fleet's power state: launch each region's shortfall (spot, then on demand in
/// the same pass), terminate what is over, report the instances while a wake's hosts come
/// up, and terminate every host while the fleet sleeps. `draining` carries the host of each
/// spot interruption notice an agent reported.
pub async fn run(
    cfg: HostConfig,
    mut power: watch::Receiver<String>,
    mut draining: tokio::sync::mpsc::UnboundedReceiver<String>,
    handle: Handle,
) {
    let mut watch: Vec<Watch> = cfg.regions.iter().map(|_| Watch::default()).collect();
    let mut recent: HashMap<String, Recent> = HashMap::new();
    // Per region and market, a launch whose outcome is unknown: its token and count.
    let mut tokens: Tokens = HashMap::new();
    let mut last_want: Option<u32> = None;
    let mut last_expected: Option<Option<Vec<String>>> = None;
    // Every host an agent reported draining, kept until no list names it live: its agent can
    // disconnect before EC2's lists show the instance going, and counted as serving then it
    // would put the region over its capacity and get its replacement terminated.
    let mut drained: BTreeSet<String> = BTreeSet::new();
    // A degraded `scale` was reported and not cleared yet.
    let mut reported = false;
    loop {
        let state = power.borrow_and_update().clone();
        while let Ok(id) = draining.try_recv() {
            drained.insert(id);
        }
        let want = if state == "asleep" {
            0
        } else {
            cfg.awake_hosts
        };
        let changed = last_want != Some(want);
        if changed {
            let since = unix_ms();
            for w in watch.iter_mut() {
                *w = Watch {
                    coming_up: want > 0,
                    since: (want > 0).then_some(since),
                    next_launch: None,
                    quota: None,
                };
            }
            last_want = Some(want);
            tracing::info!(power = %state, hosts_per_region = want, class = cfg.class.noun(), "hosts set for the power state");
        }
        let now = Instant::now();
        // Tokens are kept across power changes (an asleep pass terminates what a lost launch
        // started), but not for ever.
        tokens.retain(|(region, market), (_, count, first, last)| {
            let keep = token_step(*first, *last, now, cfg.lost_launch_ttl) != TokenStep::Drop;
            if !keep {
                tracing::warn!(%region, market = market.name(), hosts = *count,
                    "a lost {} launch got no answer in time; the hosts lists name what it launched", cfg.class.noun());
            }
            keep
        });
        // Whether this pass reports the instances: decided before any region's watch ends, so
        // the pass that sees a wake's last host running still publishes it, after the EC2 half
        // alone made the fleet awake too.
        let watching = state == "waking" || watch.iter().any(|w| w.coming_up);
        let mut errors: Vec<String> = Vec::new();
        // Every region's hosts; `None` for a region that could not be listed.
        let mut listed: Vec<Option<Vec<Host>>> = Vec::new();
        for region in &cfg.regions {
            match cfg.driver.hosts(region).await {
                Ok(h) => listed.push(Some(with_recent(region, h, &mut recent, now))),
                Err(e) => {
                    tracing::warn!(%region, error = %format!("{e:#}"), "listing the {}s failed", cfg.class.noun());
                    errors.push(format!("{region}: {e:#}"));
                    listed.push(None);
                }
            }
        }
        if listed.iter().all(Option::is_some) {
            drained.retain(|id| {
                listed
                    .iter()
                    .flatten()
                    .flatten()
                    .any(|h| h.id == *id && h.live())
            });
        }
        // With one awake host per region a notice changes nothing here: the replacement is
        // launched when EC2 takes the instance, since a region's spot quota usually holds one
        // host and a launch beside it would fall back to on demand.
        let leaving = if cfg.awake_hosts > 1 {
            drained.clone()
        } else {
            BTreeSet::new()
        };
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
                .map(|(_, (_, c, _, _))| *c)
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
            // asleep, and what is over the awake capacity. A draining host goes once the
            // region runs its awake hosts without it: its GPUs left the wall at the notice.
            let kept = serving(hosts, &leaving);
            let replaced = kept.iter().filter(|h| h.state == "running").count() as u32 >= want;
            let mut ids: Vec<String> = hosts
                .iter()
                .filter(|h| {
                    h.present() && (want == 0 || !h.live() || (replaced && leaving.contains(&h.id)))
                })
                .map(|h| h.id.clone())
                .collect();
            if want > 0 {
                ids.extend(excess(&kept, want));
            }
            if !ids.is_empty() {
                busy = true;
                match cfg.driver.terminate(region, &ids).await {
                    Ok(()) => {
                        tracing::info!(%region, instances = ?ids, power = %state, "terminating {}s", cfg.class.noun())
                    }
                    Err(e) => {
                        tracing::warn!(%region, error = %format!("{e:#}"), "terminating {}s failed", cfg.class.noun());
                        errors.push(format!("{region}: {e:#}"));
                    }
                }
            }
            if want == 0 {
                // A launch whose answer was lost may have launched a host that no list names
                // yet: ask again with its token to learn it, and terminate it now.
                let lost = reconcile(
                    &cfg,
                    region,
                    hosts,
                    &mut tokens,
                    &mut on_demand,
                    &mut recent,
                    &mut errors,
                )
                .await
                .unwrap_or_default();
                let lost: Vec<String> = lost.into_iter().filter(|id| !ids.contains(id)).collect();
                if !lost.is_empty() {
                    busy = true;
                    match cfg.driver.terminate(region, &lost).await {
                        Ok(()) => {
                            tracing::info!(%region, instances = ?lost, power = %state, "terminating {}s of lost launches", cfg.class.noun())
                        }
                        Err(e) => {
                            tracing::warn!(%region, error = %format!("{e:#}"), "terminating {}s failed", cfg.class.noun());
                            errors.push(format!("{region}: {e:#}"));
                        }
                    }
                }
                continue;
            }
            let mut live = kept.len() as u32;
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
                    let found = found.len() as u32;
                    live += found;
                    busy |= found > 0;
                    if live < want {
                        let short = want - live;
                        let mut quota = None;
                        let mut pass = Pass {
                            on_demand: &mut on_demand,
                            recent: &mut recent,
                            tokens: &mut tokens,
                            errors: &mut errors,
                            quota: &mut quota,
                        };
                        let (launched, retry) = match cfg.spread {
                            HostSpread::None => {
                                let (zones, retry) =
                                    launch_shortfall(&cfg, region, short, &[], &mut pass).await;
                                (zones.len() as u32, retry)
                            }
                            HostSpread::Az => {
                                launch_spread(&cfg, region, short, hosts, &leaving, &mut pass).await
                            }
                        };
                        watch[i].next_launch =
                            (launched < short && retry).then(|| now + cfg.capacity_retry);
                        // This attempt's answer replaces the last one's.
                        watch[i].quota = quota;
                    }
                }
            }
            if live >= want {
                watch[i].quota = None;
            }
            if let Some(q) = &watch[i].quota {
                errors.push(q.clone());
            }
            let running = |m: Market| {
                kept.iter()
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
        // The hosts kept for the awake capacity, once every region is listed and holds that
        // many: the side then needs no place for a GPU no connected host listed (#1517).
        // Only the GPU hosts are the wall's: pages mode's host has no GPU, and is no host of
        // a wake's record.
        let gpu = cfg.class == HostClass::Gpu;
        let expected = expected_hosts(&listed, want, &leaving);
        if gpu && last_expected.as_ref() != Some(&expected) {
            handle
                .send(Event::HostsExpected {
                    at: 0,
                    hosts: expected.clone(),
                })
                .await;
            last_expected = Some(expected);
        }
        if gpu && watching {
            handle
                .send(Event::ScaleInstances {
                    at: 0,
                    instances: report,
                })
                .await;
        }
        if !errors.is_empty() {
            reported = true;
            degraded(&handle, cfg.class.component(), &errors.join("; ")).await;
        } else if reported || changed {
            reported = false;
            healthy(&handle, cfg.class.component()).await;
        }
        let wait = if state == "waking" || busy || !errors.is_empty() {
            cfg.poll
        } else if want == 0 {
            // A lost launch, or a launched host no list has shown going, is followed at `poll`.
            if tokens.is_empty() && recent.is_empty() {
                ASLEEP_CHECK
            } else {
                cfg.poll
            }
        } else {
            cfg.liveness
        };
        tokio::select! {
            r = power.changed() => if r.is_err() { return },
            id = draining.recv() => match id {
                Some(id) => { drained.insert(id); }
                None => return,
            },
            _ = tokio::time::sleep(wait) => {}
        }
    }
}

/// Ask `region`'s lost launches again with their tokens. Returns the instances they launched
/// that `hosts` does not list yet (now followed as pending), or `None` when one still has no
/// answer (asked again at the next poll, and nothing new is launched).
async fn reconcile(
    cfg: &HostConfig,
    region: &str,
    hosts: &[Host],
    tokens: &mut Tokens,
    on_demand: &mut Option<u32>,
    recent: &mut HashMap<String, Recent>,
    errors: &mut Vec<String>,
) -> Option<Vec<String>> {
    let mut found = Vec::new();
    let mut held = false;
    for market in [Market::Spot, Market::OnDemand] {
        let key = (region.to_string(), market);
        let Some((token, count, first, last)) = tokens.get(&key).cloned() else {
            continue;
        };
        let asked = Instant::now();
        if token_step(first, last, asked, cfg.lost_launch_ttl) != TokenStep::Ask {
            held = true;
            continue;
        }
        if let Some(t) = tokens.get_mut(&key) {
            t.3 = asked;
        }
        // A known token sends its first request's parameters again, zones included.
        match cfg.driver.launch(region, market, count, &token, &[]).await {
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
                            zone: got.zones.get(id).cloned(),
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
                found.extend(new.into_iter().cloned());
            }
            Err(e) => {
                tracing::warn!(%region, market = market.name(), error = %format!("{e:#}"), "asking a lost {} launch again failed", cfg.class.noun());
                errors.push(format!("{region} {}: {e:#}", market.name()));
                return None;
            }
        }
    }
    // A held token's last ask may have launched what no list names yet: launch nothing new.
    (!held).then_some(found)
}

/// What one pass of [`run`] carries through its launches.
struct Pass<'a> {
    /// The fleet's on-demand hosts, `None` when a region could not be listed.
    on_demand: &'a mut Option<u32>,
    recent: &'a mut HashMap<String, Recent>,
    tokens: &'a mut Tokens,
    errors: &'a mut Vec<String>,
    /// Set when a launch is left short by the account's quota (see [`QUOTA_CODES`]).
    quota: &'a mut Option<String>,
}

/// Launch `short` hosts in `region`, in the subnets of `zones` (every subnet when empty):
/// spot, then what spot refused for capacity on demand within the fleet's on-demand cap.
/// Returns the zone of each host launched (`None` where the answer did not place it) and
/// whether the rest waits for capacity (as opposed to an error, retried at the next poll).
async fn launch_shortfall(
    cfg: &HostConfig,
    region: &str,
    short: u32,
    zones: &[String],
    pass: &mut Pass<'_>,
) -> (Vec<Option<String>>, bool) {
    let Pass {
        on_demand,
        recent,
        tokens,
        errors,
        quota: quota_refusal,
    } = pass;
    // Only this attempt's answer counts.
    **quota_refusal = None;
    let mut placed: Vec<Option<String>> = Vec::new();
    let mut launched = 0;
    let mut refused = Vec::new();
    // The market `refused` is from.
    let mut last = Market::Spot;
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
            let Some(n) = **on_demand else {
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
        let got = match cfg.driver.launch(region, market, ask, &token, zones).await {
            Ok(g) => g,
            Err(e) => {
                // It may have launched: [`reconcile`] asks again with this token, and an
                // on-demand count is reserved against the cap until then.
                tokens.insert((region.to_string(), market), (token, ask, asked, asked));
                if market == Market::OnDemand {
                    if let Some(n) = on_demand.as_mut() {
                        *n += ask;
                    }
                }
                tracing::warn!(%region, market = market.name(), error = %format!("{e:#}"), "launching {}s failed", cfg.class.noun());
                errors.push(format!("{region} {}: {e:#}", market.name()));
                return (placed, false);
            }
        };
        let request_ms = asked.elapsed().as_millis() as u64;
        for id in &got.ids {
            let zone = got.zones.get(id).cloned();
            tracing::info!(%region, instance = %id, market = market.name(), zone = ?zone, request_ms, "wake: host launch accepted");
            recent.insert(
                id.clone(),
                Recent {
                    region: region.to_string(),
                    market,
                    zone: zone.clone(),
                    asked,
                    running_reported: false,
                },
            );
            placed.push(zone);
        }
        if market == Market::OnDemand {
            if let Some(n) = on_demand.as_mut() {
                *n += got.ids.len() as u32;
            }
        }
        launched += got.ids.len() as u32;
        if !got.failed.is_empty() {
            let why = got.failed.join("; ");
            tracing::warn!(%region, market = market.name(), error = %why, "EC2 refused a {} launch", cfg.class.noun());
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
        last = market;
    }
    if launched < short {
        // `refused` is the last market's: on demand's, when spot fell back to it.
        tracing::warn!(%region, launched, short, zones = ?zones, reason = %refused.join("; "),
            "{}s still short; asking again after the capacity retry", cfg.class.noun());
        // A quota refusal is this account's limit, not EC2's capacity: it holds until the
        // quota is raised or the account's other hosts finish shutting down. It is reported
        // (the `scale` failure, kept by `run` until an attempt is not refused for quota, and
        // the line the degraded alarms match), and the region still asks again after the
        // capacity retry.
        let quota: Vec<&str> = refused
            .iter()
            .map(String::as_str)
            .filter(|r| QUOTA_CODES.iter().any(|c| r.split(':').next() == Some(*c)))
            .collect();
        if !quota.is_empty() {
            let why = quota.join("; ");
            tracing::warn!(%region, market = last.name(), error = %why, "EC2 refused a {} launch", cfg.class.noun());
            **quota_refusal = Some(format!("{region} {}: {why}", last.name()));
        }
    }
    (placed, !failed)
}

/// The zones a spread launch tries, in order: those no live host of the region is in, then
/// those no serving (not draining) host is in, then every zone (empty: no restriction). A
/// tier equal to every zone, or empty, is left out; the last tier is always every zone.
pub fn zone_tiers(
    zones: &[String],
    every: &BTreeSet<String>,
    serving: &BTreeSet<String>,
) -> Vec<Vec<String>> {
    let mut tiers: Vec<Vec<String>> = Vec::new();
    for avoid in [every, serving] {
        let t: Vec<String> = zones
            .iter()
            .filter(|z| !avoid.contains(*z))
            .cloned()
            .collect();
        if !t.is_empty() && t.len() < zones.len() && !tiers.contains(&t) {
            tiers.push(t);
        }
    }
    tiers.push(Vec::new());
    tiers
}

/// Launch `short` hosts in `region` one at a time, each into the first tier of
/// [`zone_tiers`] that has capacity, counting the zone of each host launched before the
/// next. Returns how many were launched and whether the rest waits for capacity.
async fn launch_spread(
    cfg: &HostConfig,
    region: &str,
    short: u32,
    hosts: &[Host],
    leaving: &BTreeSet<String>,
    pass: &mut Pass<'_>,
) -> (u32, bool) {
    let zones = match cfg.driver.zones(region).await {
        Ok(z) => z,
        Err(e) => {
            // Without the zones, launch over every subnet rather than not at all.
            tracing::warn!(%region, error = %format!("{e:#}"), "the host subnets' zones could not be read; launching without the spread");
            pass.errors.push(format!("{region} zones: {e:#}"));
            Vec::new()
        }
    };
    let mut every: BTreeSet<String> = BTreeSet::new();
    let mut kept: BTreeSet<String> = BTreeSet::new();
    let recent = pass
        .recent
        .iter()
        .filter(|(_, r)| r.region == region)
        .filter_map(|(id, r)| Some((id.as_str(), r.zone.as_deref()?)));
    for (id, zone) in hosts
        .iter()
        .filter(|h| h.live())
        .filter_map(|h| Some((h.id.as_str(), h.zone.as_deref()?)))
        .chain(recent)
    {
        every.insert(zone.to_string());
        if !leaving.contains(id) {
            kept.insert(zone.to_string());
        }
    }
    let mut launched = 0;
    for _ in 0..short {
        let mut placed = false;
        for tier in zone_tiers(&zones, &every, &kept) {
            let (got, retry) = launch_shortfall(cfg, region, 1, &tier, pass).await;
            if let Some(zone) = got.into_iter().next() {
                if let Some(z) = zone {
                    every.insert(z.clone());
                    kept.insert(z);
                }
                placed = true;
                break;
            }
            if !retry {
                return (launched, false);
            }
        }
        if !placed {
            break;
        }
        launched += 1;
    }
    (launched, true)
}

async fn degraded(handle: &Handle, component: &str, reason: &str) {
    handle
        .send(Event::Degraded {
            at: 0,
            component: component.into(),
            reason: Some(reason.to_string()),
        })
        .await;
}

async fn healthy(handle: &Handle, component: &str) {
    handle
        .send(Event::Degraded {
            at: 0,
            component: component.into(),
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

/// The [`CAPACITY_CODES`] that are this account's quotas rather than EC2's capacity: a pass
/// left short by one of them marks the coordinator degraded.
pub const QUOTA_CODES: &[&str] = &["VcpuLimitExceeded", "MaxSpotInstanceCountExceeded"];

/// EC2 on AWS: `CreateFleet` (type `instant`) over the region's host templates and
/// subnets, `DescribeInstances` by tag, `TerminateInstances`.
pub struct AwsHosts {
    /// The host launch templates of each region.
    pub templates: BTreeMap<String, Vec<String>>,
    /// The subnets of each region hosts may launch in.
    pub subnets: BTreeMap<String, Vec<String>>,
    /// Tagged on every host (`Fleet=<name>`); only this fleet's hosts are listed.
    pub fleet: String,
    /// Tagged on every host as `Purpose=<class purpose>`; only the class's hosts are listed.
    pub class: HostClass,
    /// Per-region API base; default `https://ec2.<region>.amazonaws.com`.
    pub endpoints: BTreeMap<String, String>,
    /// How long a template's instance type is reused before it is read again.
    pub template_type_ttl: Duration,
    /// Each template's instance type, as its validated default says, and when it was read.
    types: Mutex<HashMap<(String, String), (String, Instant)>>,
    /// By ClientToken, the parameters of each CreateFleet that got no decision, and when they
    /// were sent. A retry with the token sends them again unchanged: EC2 refuses a token with
    /// other parameters (IdempotentParameterMismatch), and the template's instance type can
    /// change between the two requests.
    unanswered: Mutex<HashMap<String, (QueryParams, Instant)>>,
    /// Per region, each host subnet's availability zone, read once (a subnet never moves).
    subnet_zones: Mutex<HashMap<String, BTreeMap<String, String>>>,
    chain: CredentialChain,
    http: reqwest::Client,
}

/// How long a template's instance type is reused before it is read again, by default.
const TEMPLATE_TYPE_TTL: Duration = Duration::from_secs(300);

/// An EC2 query request's parameters, in order.
type QueryParams = Vec<(String, String)>;

/// How long an unanswered CreateFleet's parameters are kept for a retry with its token.
const UNANSWERED_TTL: Duration = Duration::from_secs(24 * 3600);

impl AwsHosts {
    /// The GPU hosts' driver.
    pub fn new(templates: &[Regional], subnets: &[Regional], fleet: String) -> Result<Self> {
        AwsHosts::for_class(templates, subnets, fleet, HostClass::Gpu)
    }

    /// The driver of `class`'s hosts.
    pub fn for_class(
        templates: &[Regional],
        subnets: &[Regional],
        fleet: String,
        class: HostClass,
    ) -> Result<Self> {
        let (template_flag, subnet_flag) = class.flags();
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
                "{template_flag} in {region} needs a {subnet_flag} in {region}"
            );
        }
        for region in s.keys() {
            anyhow::ensure!(
                t.contains_key(region),
                "{subnet_flag} in {region} has no {template_flag} in {region}"
            );
        }
        Ok(AwsHosts {
            templates: t,
            subnets: s,
            fleet,
            class,
            endpoints: BTreeMap::new(),
            template_type_ttl: TEMPLATE_TYPE_TTL,
            types: Mutex::new(HashMap::new()),
            unanswered: Mutex::new(HashMap::new()),
            subnet_zones: Mutex::new(HashMap::new()),
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

    /// The instance type of `template`'s validated default in `region`, read at most once per
    /// `template_type_ttl`.
    async fn template_type(&self, region: &str, template: &str) -> Result<String> {
        let key = (region.to_string(), template.to_string());
        if let Some((ty, at)) = self.types.lock().unwrap().get(&key) {
            if at.elapsed() < self.template_type_ttl {
                return Ok(ty.clone());
            }
        }
        let xml = self
            .call_ok(
                region,
                &[
                    p("Action", "DescribeLaunchTemplateVersions"),
                    p("LaunchTemplateName", template),
                    p("LaunchTemplateVersion.1", "$Default"),
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

    /// Each host subnet of `region` and its availability zone (DescribeSubnets), read once.
    async fn subnet_zones(&self, region: &str) -> Result<BTreeMap<String, String>> {
        if let Some(z) = self.subnet_zones.lock().unwrap().get(region) {
            return Ok(z.clone());
        }
        let subnets = self
            .subnets
            .get(region)
            .with_context(|| format!("no host subnet in {region}"))?;
        let mut params = vec![p("Action", "DescribeSubnets")];
        for (i, s) in subnets.iter().enumerate() {
            params.push(p(format!("SubnetId.{}", i + 1), s.as_str()));
        }
        let xml = self
            .call_ok(region, &params)
            .await
            .with_context(|| format!("reading the host subnets of {region}"))?;
        let zones = parse_subnet_zones(&xml);
        if let Some(s) = subnets.iter().find(|s| !zones.contains_key(*s)) {
            bail!("DescribeSubnets in {region} did not name the zone of {s}");
        }
        self.subnet_zones
            .lock()
            .unwrap()
            .insert(region.to_string(), zones.clone());
        Ok(zones)
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
/// `subnets`, tagged for `fleet` and `purpose` (the class's `Purpose`). `templates` pairs each template name with its instance
/// type, which every override names: IAM checks `ec2:InstanceType` on the fleet's instances,
/// and for `CreateFleet` it takes that key only from the request, not from the template.
pub fn create_fleet_params(
    templates: &[(String, String)],
    subnets: &[String],
    market: Market,
    count: u32,
    fleet: &str,
    purpose: &str,
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
            "$Default",
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
        ("Name", purpose),
        ("Purpose", purpose),
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

/// Each subnet of a DescribeSubnets answer and its availability zone.
pub fn parse_subnet_zones(xml: &str) -> BTreeMap<String, String> {
    set_items(xml, "subnetSet")
        .into_iter()
        .filter_map(|seg| Some((xml_tag(seg, "subnetId")?, xml_tag(seg, "availabilityZone")?)))
        .collect()
}

/// A `CreateFleet` (instant) answer: the instances launched, and its errors split into
/// capacity refusals and everything else, each `<code>: <message>`, without repeats. Each
/// instance's zone is its subnet's in `subnet_zones` (the answer names the override's subnet).
pub fn parse_create_fleet(xml: &str, subnet_zones: &BTreeMap<String, String>) -> Launch {
    let mut out = Launch {
        ids: items_of(xml, "instanceIds"),
        ..Launch::default()
    };
    for item in set_items(xml, "fleetInstanceSet") {
        let Some(zone) = xml_tag(item, "subnetId").and_then(|s| subnet_zones.get(&s).cloned())
        else {
            continue;
        };
        for id in items_of(item, "instanceIds") {
            out.zones.insert(id, zone.clone());
        }
    }
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
/// The text of every `<name>` element in `xml`.
fn xml_all(xml: &str, name: &str) -> Vec<String> {
    let (open, close) = (format!("<{name}>"), format!("</{name}>"));
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find(&open) {
        let body = &rest[i + open.len()..];
        let Some(end) = body.find(&close) else { break };
        out.push(body[..end].trim().to_string());
        rest = &body[end..];
    }
    out
}

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
                sources: ["ipAddress", "publicIp", "ipv6Address", "ipv6Prefix"]
                    .iter()
                    .flat_map(|t| xml_all(seg, t))
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .collect(),
                zone: xml_tag(seg, "placement").and_then(|p| xml_tag(&p, "availabilityZone")),
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
                p("Filter.1.Value.1", self.class.purpose()),
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

    async fn zones(&self, region: &str) -> Result<Vec<String>> {
        let zones = self.subnet_zones(region).await?;
        let mut out: Vec<String> = Vec::new();
        for s in self.subnets.get(region).into_iter().flatten() {
            if let Some(z) = zones.get(s).filter(|z| !out.contains(z)) {
                out.push(z.clone());
            }
        }
        Ok(out)
    }

    async fn launch(
        &self,
        region: &str,
        market: Market,
        count: u32,
        token: &str,
        zones: &[String],
    ) -> Result<Launch> {
        let kept = self
            .unanswered
            .lock()
            .unwrap()
            .get(token)
            .map(|(params, _)| params.clone());
        let params = match kept {
            Some(params) => params,
            None => {
                let templates = self
                    .templates
                    .get(region)
                    .with_context(|| format!("no host template in {region}"))?;
                let subnets = self
                    .subnets
                    .get(region)
                    .with_context(|| format!("no host subnet in {region}"))?;
                // The zones come from `zones`, which read the subnets' zones; a request that
                // names none of them launches over every subnet.
                let by_zone = self
                    .subnet_zones
                    .lock()
                    .unwrap()
                    .get(region)
                    .cloned()
                    .unwrap_or_default();
                let within: Vec<String> = subnets
                    .iter()
                    .filter(|s| by_zone.get(*s).is_some_and(|z| zones.contains(z)))
                    .cloned()
                    .collect();
                let subnets = if within.is_empty() {
                    subnets.clone()
                } else {
                    within
                };
                let mut typed = Vec::with_capacity(templates.len());
                for t in templates {
                    typed.push((t.clone(), self.template_type(region, t).await?));
                }
                create_fleet_params(
                    &typed,
                    &subnets,
                    market,
                    count,
                    &self.fleet,
                    self.class.purpose(),
                    token,
                )
            }
        };
        let answer = self.call(region, &params).await;
        // Zones are named when the subnets' zones were read (`zones`, the spread's first call).
        let subnet_zones = self
            .subnet_zones
            .lock()
            .unwrap()
            .get(region)
            .cloned()
            .unwrap_or_default();
        let mut unanswered = self.unanswered.lock().unwrap();
        match answer {
            Ok(xml) => {
                unanswered.remove(token);
                Ok(parse_create_fleet(&xml, &subnet_zones))
            }
            // A request refused as a whole for capacity is a refusal like a per-pool one.
            Err((code, msg)) if CAPACITY_CODES.contains(&code.as_str()) => {
                unanswered.remove(token);
                Ok(Launch {
                    refused: vec![format!("{code}: {msg}")],
                    ..Launch::default()
                })
            }
            // No decision: the caller asks again with this token, and gets these parameters.
            Err((code, msg)) => {
                unanswered.retain(|_, (_, at)| at.elapsed() < UNANSWERED_TTL);
                unanswered
                    .entry(token.to_string())
                    .or_insert_with(|| (params, Instant::now()));
                bail!("CreateFleet: {code}: {msg}")
            }
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
/// unlimited); a launch beyond it is refused with `InsufficientInstanceCapacity`, or with the
/// market's entry in `refusal` (e.g. `VcpuLimitExceeded: ...`) when it has one. Launched
/// hosts start in `launch_state` (default `pending`); `set_state` moves them. Every call is
/// recorded in `calls`, in order: `list <region>`, `launch <region> <market> <count>`,
/// `terminate <region> <ids>`. `fail_lists` makes that many next lists of a region fail.
/// A launch is idempotent by its token. `ambiguous` makes that many next launches that launch
/// something fail after launching, as a request whose answer is lost. `hide_new` (show, hide): each launched
/// instance is named by its region's next `show` lists, left out of the `hide` after them,
/// then named again, as an eventually consistent DescribeInstances. `stuck` makes that many
/// next launches with a new token fail without launching, and every later request with one of
/// their tokens fail too, as a retry EC2 never answers. `zones` are each region's
/// availability zones: a launch places each instance in the first of the zones it may use
/// (the request's, or every zone of the region) that `zone_capacity` (region, zone, market;
/// unlimited when absent) still has room in, and records ` in <zones>` when it named zones.
#[derive(Default)]
pub struct FakeHosts {
    pub hosts: Mutex<BTreeMap<String, Vec<Host>>>,
    pub capacity: Mutex<HashMap<(String, Market), u32>>,
    pub zones: Mutex<HashMap<String, Vec<String>>>,
    pub zone_capacity: Mutex<HashMap<(String, String, Market), u32>>,
    /// Every list waits for this lock after recording its call: a test that holds it keeps
    /// the launcher inside a list.
    pub hold_lists: Arc<tokio::sync::Mutex<()>>,
    pub launch_state: Mutex<Option<String>>,
    pub calls: Mutex<Vec<String>>,
    pub fail_lists: Mutex<HashMap<String, u32>>,
    pub ambiguous: Mutex<u32>,
    pub hide_new: Mutex<Option<(u32, u32)>>,
    pub stuck: Mutex<u32>,
    pub refusal: Mutex<HashMap<Market, String>>,
    stuck_tokens: Mutex<std::collections::HashSet<String>>,
    by_token: Mutex<HashMap<String, Launch>>,
    visibility: Mutex<HashMap<String, (u32, u32)>>,
    next: std::sync::atomic::AtomicU64,
}

impl FakeHosts {
    /// Hosts whose instance ids start at `first` (`i-` and 17 hex digits), so two fakes, one
    /// per host group, mint ids that differ as two classes' instances do.
    pub fn numbered_from(first: u64) -> Self {
        FakeHosts {
            next: std::sync::atomic::AtomicU64::new(first),
            ..FakeHosts::default()
        }
    }

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

    /// The zone of each live host of `region`, by instance id.
    pub fn live_zones(&self, region: &str) -> BTreeMap<String, Option<String>> {
        self.present(region)
            .into_iter()
            .filter(|h| h.live())
            .map(|h| (h.id, h.zone))
            .collect()
    }
}

#[async_trait]
impl HostDriver for FakeHosts {
    async fn hosts(&self, region: &str) -> Result<Vec<Host>> {
        self.calls.lock().unwrap().push(format!("list {region}"));
        drop(self.hold_lists.lock().await);
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

    async fn zones(&self, region: &str) -> Result<Vec<String>> {
        Ok(self
            .zones
            .lock()
            .unwrap()
            .get(region)
            .cloned()
            .unwrap_or_default())
    }

    async fn launch(
        &self,
        region: &str,
        market: Market,
        count: u32,
        token: &str,
        zones: &[String],
    ) -> Result<Launch> {
        let named = if zones.is_empty() {
            String::new()
        } else {
            format!(" in {}", zones.join(","))
        };
        self.calls
            .lock()
            .unwrap()
            .push(format!("launch {region} {} {count}{named}", market.name()));
        if let Some(l) = self.by_token.lock().unwrap().get(token) {
            return Ok(l.clone());
        }
        {
            let mut stuck = self.stuck.lock().unwrap();
            let mut tokens = self.stuck_tokens.lock().unwrap();
            if *stuck > 0 && !tokens.contains(token) {
                *stuck -= 1;
                tokens.insert(token.to_string());
            }
            if tokens.contains(token) {
                bail!("injected: no answer for this token");
            }
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
        let allowed: Vec<String> = if zones.is_empty() {
            self.zones
                .lock()
                .unwrap()
                .get(region)
                .cloned()
                .unwrap_or_default()
        } else {
            zones.to_vec()
        };
        let mut out = Launch::default();
        let mut placed = 0;
        for _ in 0..n {
            // The first allowed zone with room; none at all when the region has no zones.
            let zone = {
                let mut cap = self.zone_capacity.lock().unwrap();
                let mut pick = None;
                for z in &allowed {
                    match cap.get_mut(&(region.to_string(), z.clone(), market)) {
                        Some(0) => continue,
                        Some(c) => {
                            *c -= 1;
                            pick = Some(z.clone());
                            break;
                        }
                        None => {
                            pick = Some(z.clone());
                            break;
                        }
                    }
                }
                if pick.is_none() && !allowed.is_empty() {
                    continue;
                }
                pick
            };
            placed += 1;
            let id = format!(
                "i-{:017x}",
                self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            );
            if let Some(z) = &zone {
                out.zones.insert(id.clone(), z.clone());
            }
            self.hosts
                .lock()
                .unwrap()
                .entry(region.to_string())
                .or_default()
                .push(Host {
                    id: id.clone(),
                    state: state.clone(),
                    market,
                    sources: Vec::new(),
                    zone,
                });
            if let Some(v) = *self.hide_new.lock().unwrap() {
                self.visibility.lock().unwrap().insert(id.clone(), v);
            }
            out.ids.push(id);
        }
        if placed < n {
            // What the zones had no room for stays in the region's capacity.
            if let Some(c) = self
                .capacity
                .lock()
                .unwrap()
                .get_mut(&(region.to_string(), market))
            {
                *c += n - placed;
            }
        }
        if placed < count {
            let why = self.refusal.lock().unwrap().get(&market).cloned();
            out.refused.push(why.unwrap_or_else(|| {
                format!(
                    "InsufficientInstanceCapacity: no {} capacity",
                    market.name()
                )
            }));
        }
        self.by_token
            .lock()
            .unwrap()
            .insert(token.to_string(), out.clone());
        let mut amb = self.ambiguous.lock().unwrap();
        if *amb > 0 && placed > 0 {
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

    /// A token whose last ask launched a host just before its time ran out is held, not
    /// dropped: dropping it then would release its on-demand reservation while that host is
    /// still missing from the lists, and another region could take the place (Codex on #1585).
    #[test]
    fn a_token_is_held_after_its_last_ask_until_a_list_can_name_what_it_launched() {
        let ttl = Duration::from_secs(120);
        let first = Instant::now();
        let at = |s: u64| first + Duration::from_secs(s);
        assert_eq!(token_step(first, first, at(119), ttl), TokenStep::Ask);
        // Asked last at 118 s: asked no more at 120 s, but held until 238 s.
        assert_eq!(token_step(first, at(118), at(120), ttl), TokenStep::Hold);
        assert_eq!(token_step(first, at(118), at(237), ttl), TokenStep::Hold);
        assert_eq!(token_step(first, at(118), at(238), ttl), TokenStep::Drop);
        // A token that never got an answer and was last asked at 119 s goes at 239 s.
        assert_eq!(token_step(first, at(119), at(239), ttl), TokenStep::Drop);
    }

    fn host(id: &str, state: &str, market: Market) -> Host {
        Host {
            id: id.into(),
            state: state.into(),
            market,
            sources: Vec::new(),
            zone: None,
        }
    }

    /// One launch pass for one host in a region where both markets have no room, on demand
    /// refused with `od_refusal` (EC2's capacity refusal when `None`): what it logged, the
    /// errors it reported, the quota refusal it reported, and whether the rest waits for
    /// capacity.
    async fn short_pass(od_refusal: Option<&str>) -> (String, Vec<String>, Option<String>, bool) {
        #[derive(Clone)]
        struct Buf(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Buf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let buf = Buf(Arc::new(Mutex::new(Vec::new())));
        let w = buf.clone();
        let sub = tracing_subscriber::fmt()
            .with_writer(move || w.clone())
            .with_ansi(false)
            .finish();
        let _guard = tracing::subscriber::set_default(sub);
        let fake = Arc::new(FakeHosts::default());
        for m in [Market::Spot, Market::OnDemand] {
            fake.capacity.lock().unwrap().insert(("r".into(), m), 0);
        }
        if let Some(r) = od_refusal {
            fake.refusal
                .lock()
                .unwrap()
                .insert(Market::OnDemand, r.into());
        }
        let cfg = HostConfig {
            class: HostClass::Gpu,
            driver: fake.clone(),
            regions: vec!["r".into()],
            awake_hosts: 1,
            spread: HostSpread::None,
            on_demand_max: 8,
            poll: Duration::from_millis(50),
            liveness: Duration::from_millis(50),
            capacity_retry: Duration::from_millis(50),
            lost_launch_ttl: Duration::from_secs(120),
        };
        let (mut on_demand, mut recent, mut tokens, mut errors, mut quota) =
            (Some(0), HashMap::new(), HashMap::new(), Vec::new(), None);
        let mut pass = Pass {
            on_demand: &mut on_demand,
            recent: &mut recent,
            tokens: &mut tokens,
            errors: &mut errors,
            quota: &mut quota,
        };
        let (launched, retry) = launch_shortfall(&cfg, "r", 1, &[], &mut pass).await;
        assert!(launched.is_empty());
        let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        (out, errors, quota, retry)
    }

    /// A pass that leaves a region short says why its last market refused: on 2026-11-05
    /// the wall's host waited 13 minutes and the log named only spot's refusal, while on
    /// demand was refused with VcpuLimitExceeded (#1257).
    #[tokio::test(flavor = "current_thread")]
    async fn a_region_still_short_logs_the_on_demand_refusal() {
        let (out, errors, quota, retry) = short_pass(None).await;
        let line = out
            .lines()
            .find(|l| l.contains("GPU hosts still short"))
            .unwrap_or_else(|| panic!("{out}"));
        assert!(line.contains("no on-demand capacity"), "{line}");
        // EC2 had no capacity: the region waits for it, and nothing is degraded.
        assert!(retry);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(quota, None);
        assert!(!out.contains("EC2 refused a GPU host launch"), "{out}");
    }

    /// A launch that the account's vCPU quota refuses is reported, not only retried. On
    /// 2026-11-22 staging's wake was refused on demand with VcpuLimitExceeded every 10 s for
    /// 15 minutes (the G quota was 384 vCPUs, production held two hosts and terminated hosts
    /// were still shutting down), and nothing but a WARN line said so: no `scale` failure in
    /// the state and no match for the coordinator's degraded alarm, which matches
    /// "EC2 refused a GPU host launch". The same refusal had blocked staging's wakes for
    /// 12 to 27 minutes at a time on 2026-11-07.
    #[tokio::test(flavor = "current_thread")]
    async fn an_on_demand_quota_refusal_marks_the_scale_degraded() {
        let quota = "VcpuLimitExceeded: You have requested more vCPU capacity than your \
                     current vCPU limit of 384 allows";
        let (out, errors, reported, retry) = short_pass(Some(quota)).await;
        // The quota can free (a terminated host finishes shutting down): ask again after the
        // capacity retry, as for capacity. `run` reports it as degraded
        // (a_quota_refused_launch_stays_degraded_between_attempts).
        assert!(retry);
        assert!(errors.is_empty(), "{errors:?}");
        let reported = reported.expect("the quota refusal is reported");
        assert!(
            reported.starts_with("r on-demand: VcpuLimitExceeded"),
            "{reported}"
        );
        let line = out
            .lines()
            .find(|l| l.contains("EC2 refused a GPU host launch"))
            .unwrap_or_else(|| panic!("{out}"));
        assert!(line.contains("VcpuLimitExceeded"), "{line}");
    }

    /// Every region listed and at its capacity: its live hosts less the excess. A region
    /// unlisted or short, or asleep, says nothing.
    #[test]
    fn expected_hosts_are_the_kept_hosts_of_full_regions_only() {
        let a = vec![
            host("i-2", "running", Market::Spot),
            host("i-1", "pending", Market::OnDemand),
            host("i-0", "shutting-down", Market::Spot),
        ];
        let b = vec![host("i-3", "running", Market::Spot)];
        let none = BTreeSet::new();
        assert_eq!(
            expected_hosts(&[Some(a.clone()), Some(b.clone())], 1, &none),
            Some(vec!["i-2".to_string(), "i-3".to_string()]),
            "the on-demand host over the capacity is not kept"
        );
        assert_eq!(
            expected_hosts(&[Some(a.clone()), Some(b.clone())], 2, &none),
            None
        );
        assert_eq!(expected_hosts(&[Some(a.clone()), None], 1, &none), None);
        assert_eq!(
            expected_hosts(&[Some(a.clone()), Some(b.clone())], 0, &none),
            None
        );
        assert_eq!(expected_hosts(&[Some(vec![])], 1, &none), None);
        // A draining host is not kept: its region is short until the replacement is listed.
        let leaving: BTreeSet<String> = ["i-3".to_string()].into();
        assert_eq!(expected_hosts(&[Some(a), Some(b)], 1, &leaving), None);
    }

    fn zoned(z: &[&str]) -> BTreeSet<String> {
        z.iter().map(|z| z.to_string()).collect()
    }

    /// A spread launch tries the zones no live host is in, then those no serving host is in
    /// (a draining host's zone allowed again), then every zone; a tier that is every zone or
    /// none is left out, and the last is always every zone.
    #[test]
    fn spread_tiers_avoid_live_zones_then_serving_zones() {
        let zones: Vec<String> = ["a", "b", "c"].iter().map(|z| z.to_string()).collect();
        let s = |v: &[&str]| v.iter().map(|z| z.to_string()).collect::<Vec<_>>();
        assert_eq!(
            zone_tiers(&zones, &zoned(&["a", "b"]), &zoned(&["a"])),
            vec![s(&["c"]), s(&["b", "c"]), vec![]]
        );
        // Two zones, one serving host in a and one draining in b: b first, then any.
        let two = s(&["a", "b"]);
        assert_eq!(
            zone_tiers(&two, &zoned(&["a", "b"]), &zoned(&["a"])),
            vec![s(&["b"]), vec![]]
        );
        // No host yet: every zone, once.
        let any: Vec<Vec<String>> = vec![vec![]];
        assert_eq!(zone_tiers(&zones, &zoned(&[]), &zoned(&[])), any);
        // Zones unknown: no restriction.
        assert_eq!(zone_tiers(&[], &zoned(&["a"]), &zoned(&["a"])), any);
    }

    #[test]
    fn subnets_and_launches_carry_their_zones() {
        let xml = r#"<DescribeSubnetsResponse><subnetSet>
  <item><subnetId>subnet-1</subnetId><availabilityZone>us-east-1a</availabilityZone></item>
  <item><availabilityZone>us-east-1b</availabilityZone><subnetId>subnet-2</subnetId></item>
</subnetSet></DescribeSubnetsResponse>"#;
        let zones = parse_subnet_zones(xml);
        assert_eq!(
            zones.get("subnet-2").map(String::as_str),
            Some("us-east-1b")
        );
        let fleet = r#"<CreateFleetResponse><fleetInstanceSet>
  <item><launchTemplateAndOverrides><overrides><subnetId>subnet-2</subnetId></overrides></launchTemplateAndOverrides>
    <lifecycle>spot</lifecycle><instanceIds><item>i-0123456789abcdef0</item></instanceIds></item>
</fleetInstanceSet></CreateFleetResponse>"#;
        let got = parse_create_fleet(fleet, &zones);
        assert_eq!(
            got.zones.get("i-0123456789abcdef0").map(String::as_str),
            Some("us-east-1b")
        );
        let described = r#"<DescribeInstancesResponse><reservationSet><item><instancesSet>
  <item><instanceId>i-0123456789abcdef0</instanceId><instanceState><code>16</code><name>running</name></instanceState>
    <placement><availabilityZone>us-east-1b</availabilityZone><tenancy>default</tenancy></placement></item>
</instancesSet></item></reservationSet></DescribeInstancesResponse>"#;
        assert_eq!(
            parse_hosts(described)[0].zone.as_deref(),
            Some("us-east-1b")
        );
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
            "fcvm-fleet-host",
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
            "fcvm-fleet-host",
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
        let got = parse_create_fleet(xml, &BTreeMap::new());
        assert_eq!(got.ids, vec!["i-0123456789abcdef0".to_string()]);
        assert_eq!(got.refused.len(), 1, "{got:?}");
        assert!(got.refused[0].starts_with("InsufficientInstanceCapacity: "));
        assert_eq!(got.failed.len(), 1, "{got:?}");
        let none = parse_create_fleet(
            "<CreateFleetResponse><fleetInstanceSet/></CreateFleetResponse>",
            &BTreeMap::new(),
        );
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

    /// An instance's sources are its public IPv4 address and every IPv6 address and prefix of its
    /// interfaces; its private addresses are not (a connection through Cloudflare never comes
    /// from one).
    #[test]
    fn describe_answers_carry_each_hosts_public_addresses_and_ipv6_prefixes() {
        let xml = r#"<DescribeInstancesResponse><reservationSet><item><instancesSet>
  <item><instanceId>i-0123456789abcdef0</instanceId><instanceState><code>16</code><name>running</name></instanceState>
    <privateIpAddress>10.0.0.5</privateIpAddress><ipAddress>198.51.100.7</ipAddress>
    <networkInterfaceSet><item><privateIpAddress>10.0.0.5</privateIpAddress>
      <association><publicIp>198.51.100.7</publicIp></association>
      <ipv6AddressesSet><item><ipv6Address>2001:db8:7:7::5</ipv6Address></item></ipv6AddressesSet>
      <ipv6PrefixSet><item><ipv6Prefix>2001:db8:7:7:1::/80</ipv6Prefix></item></ipv6PrefixSet>
    </item></networkInterfaceSet></item>
</instancesSet></item></reservationSet></DescribeInstancesResponse>"#;
        let h = &parse_hosts(xml)[0];
        assert_eq!(
            h.sources,
            vec!["198.51.100.7", "2001:db8:7:7:1::/80", "2001:db8:7:7::5"]
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
                zone: None,
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
                zone: None,
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
