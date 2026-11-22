//! The spend meter (`--daily-spend-usd`): what the fleet's EC2 instances cost per hour, reported
//! to the state as `Event::Burn`; the ledger and the forced sleep are `fleet_core::spend`.
//!
//! Every [`MeterConfig::every`] the meter lists this fleet's GPU hosts and EC2 tiles in every
//! region it launches in (`DescribeInstances`, `tag:Fleet=<fleet>`, `tag:Purpose` host or
//! tile, pending to shutting-down) and prices each one: a spot instance at the current spot
//! price of its type in its availability zone (`DescribeSpotPriceHistory`, cached for
//! [`MeterConfig::spot_ttl`]), an on-demand one at its type's on-demand rate
//! (`--spend-rates`, over [`DEFAULT_ON_DEMAND`]). It errs high where it cannot know: a spot
//! instance whose price is not available is priced on demand, an instance whose type has no
//! on-demand rate at the highest rate known, and a region it cannot list keeps its last
//! listed instances. Each of those marks the `spend` component degraded. The day's spend
//! reaches CloudWatch as `SpendUsdToday` (`super::metrics`).

use super::aws::{set_items, signed, xml_tag, CredentialChain};
use super::sigv4::uri_encode;
use super::Handle;
use anyhow::{Context, Result};
use async_trait::async_trait;
use fleet_core::Event;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

/// US East (N. Virginia) Linux on-demand list prices, USD per hour, of the instance types the
/// fleet launches (AWS Price List API, 2026-11-19; the same in us-east-2 and us-west-2).
pub const DEFAULT_ON_DEMAND: &[(&str, f64)] = &[
    ("g4dn.metal", 7.824),
    ("g4dn.xlarge", 0.526),
    ("g5g.metal", 2.744),
    ("g5g.xlarge", 0.42),
];

/// The instance states the meter charges for: from launch until the instance is gone or
/// stopped. A pending instance is not billed yet; it is counted so a wake's launches show at
/// once.
const BILLED_STATES: &[&str] = &["pending", "running", "stopping", "shutting-down"];

/// One instance the fleet pays for.
#[derive(Debug, Clone, PartialEq)]
pub struct Billed {
    pub id: String,
    pub instance_type: String,
    pub spot: bool,
    pub zone: String,
    /// An EC2 tile (`Purpose=fcvm-fleet-tile`); otherwise a GPU host.
    pub tile: bool,
}

/// The Lambda Managed Instances management fee for a g4dn.xlarge, USD per hour: the AWS Price
/// List's usage type `Lambda-Managed-Instances-g4dn.xlarge-Management-Hours` in us-east-1
/// (9.75% of the type's $0.526 on-demand rate), charged on top of the instance itself.
pub const LAMBDA_FEE_USD_PER_HOUR: f64 = 0.051285;
/// The instance type a Lambda tile runs on.
pub const LAMBDA_INSTANCE_TYPE: &str = "g4dn.xlarge";
/// Instances a Lambda tile keeps while it is assigned: Lambda keeps the instance the previous
/// version ran on beside the current one, and a function's scaling cannot be held to one
/// environment (measured 2026-11-22: every PutFunctionScalingConfig answered
/// ResourceConflict). The instances themselves are not visible to the fleet's role, so the
/// meter charges this many per assigned tile.
pub const LAMBDA_INSTANCES_PER_TILE: f64 = 2.0;
/// A tile that left the assignment is charged this long after: its function's delete took
/// 37 to 66 s (2026-11-22).
pub const LAMBDA_DELETE_TAIL: Duration = Duration::from_secs(120);

/// USD per hour of one Lambda instance: the type's on-demand rate (the highest rate known
/// when the type has none) plus the management fee.
pub fn lambda_instance_rate(on_demand: &BTreeMap<String, f64>) -> f64 {
    let od = on_demand
        .get(LAMBDA_INSTANCE_TYPE)
        .copied()
        .unwrap_or_else(|| on_demand.values().copied().fold(0.0, f64::max));
    od + LAMBDA_FEE_USD_PER_HOUR
}

/// A spot price: instance type, availability zone, USD per hour.
pub type SpotPrice = (String, String, f64);

#[async_trait]
pub trait MeterDriver: Send + Sync {
    /// This fleet's billed hosts and tiles in `region`.
    async fn instances(&self, region: &str) -> Result<Vec<Billed>>;
    /// The current spot prices of `types` in `region`, per availability zone.
    async fn spot_prices(&self, region: &str, types: &[String]) -> Result<Vec<SpotPrice>>;
}

pub struct MeterConfig {
    pub driver: Arc<dyn MeterDriver>,
    pub regions: Vec<String>,
    /// On-demand USD per hour by instance type.
    pub on_demand: BTreeMap<String, f64>,
    pub every: Duration,
    pub spot_ttl: Duration,
    /// A region with no successful listing for this long (or since the meter started) is
    /// unknown, and the fleet is charged at `cap_usd_per_hour`.
    pub stale_after: Duration,
    /// The most the fleet can be spending: every host and tile it may run, at the highest
    /// on-demand rate (`launch_cap_usd_per_hour`).
    pub cap_usd_per_hour: f64,
    /// With Lambda tiles: USD per hour of one Lambda instance ([`lambda_instance_rate`]).
    /// The meter then charges [`LAMBDA_INSTANCES_PER_TILE`] of them per tile assigned (or
    /// assigned within [`LAMBDA_DELETE_TAIL`]) and reports the rate split by backing.
    pub lambda_usd_per_instance_hour: Option<f64>,
}

/// What the meter charges the Lambda tiles: per tile assigned now or within the delete
/// tail, [`LAMBDA_INSTANCES_PER_TILE`] instances at `rate`. `last_seen` is updated with the
/// tiles assigned `now`.
pub fn lambda_burn(
    assigned: &[String],
    last_seen: &mut HashMap<String, Instant>,
    now: Instant,
    rate: f64,
) -> f64 {
    for t in assigned {
        last_seen.insert(t.clone(), now);
    }
    last_seen.retain(|_, at| now.duration_since(*at) < LAMBDA_DELETE_TAIL);
    last_seen.len() as f64 * LAMBDA_INSTANCES_PER_TILE * rate
}

/// How long a region's instance list is trusted without a fresh listing: ten passes of the
/// meter. Past it the list may be missing launches, so the fleet is charged at its launch cap.
pub const STALE_AFTER: Duration = Duration::from_secs(300);

/// The most the fleet can be running: in each host region its awake hosts and the
/// replacement a spot interruption notice launches, and with EC2 tiles each tile plus a
/// relaunch in flight, all at the highest on-demand rate known.
pub fn launch_cap_usd_per_hour(
    args: &crate::cli::FleetCoordArgs,
    on_demand: &BTreeMap<String, f64>,
) -> f64 {
    let mut host_regions: Vec<&str> = args
        .host_templates
        .iter()
        .map(|t| t.region.as_str())
        .collect();
    host_regions.sort();
    host_regions.dedup();
    let hosts = host_regions.len() as f64 * (args.awake_hosts as f64 + 1.0);
    let lambda_tiles = if args.lambda == crate::cli::FleetLambda::Aws {
        args.lambda_tiles.min(args.ec2_tiles)
    } else {
        0
    };
    let tiles = if args.ec2 == crate::cli::FleetEc2::Aws {
        2.0 * (args.ec2_tiles - lambda_tiles) as f64
    } else {
        0.0
    };
    // A Lambda tile: its two instances and one more while a race's version comes up.
    let lambda =
        (LAMBDA_INSTANCES_PER_TILE + 1.0) * lambda_tiles as f64 * lambda_instance_rate(on_demand);
    (hosts + tiles) * on_demand.values().copied().fold(0.0, f64::max) + lambda
}

/// The regions whose last successful listing (or, never listed, the meter's start) is at
/// least `bound` before `now`, in `regions` order.
pub fn stale_regions(
    regions: &[String],
    listed_at: &HashMap<String, Instant>,
    started: Instant,
    now: Instant,
    bound: Duration,
) -> Vec<String> {
    regions
        .iter()
        .filter(|r| now.duration_since(listed_at.get(*r).copied().unwrap_or(started)) >= bound)
        .cloned()
        .collect()
}

/// `--spend-rates`: `<instance type>=<USD per hour>` on-demand rates, over the defaults.
pub fn on_demand_rates(args: &[String]) -> Result<BTreeMap<String, f64>> {
    let mut rates: BTreeMap<String, f64> = DEFAULT_ON_DEMAND
        .iter()
        .map(|(t, r)| (t.to_string(), *r))
        .collect();
    for a in args {
        let (t, r) = a
            .split_once('=')
            .filter(|(t, _)| !t.is_empty())
            .with_context(|| {
                format!("--spend-rates {a:?} is not <instance type>=<USD per hour>")
            })?;
        let r: f64 = r
            .parse()
            .ok()
            .filter(|r: &f64| r.is_finite() && *r > 0.0)
            .with_context(|| format!("--spend-rates {a:?}: the rate is not a positive number"))?;
        rates.insert(t.to_string(), r);
    }
    Ok(rates)
}

/// The instances of a DescribeInstances answer, one per item of its `instancesSet` lists.
pub fn parse_instances(xml: &str) -> Vec<Billed> {
    set_items(xml, "instancesSet")
        .into_iter()
        .filter_map(|seg| {
            Some(Billed {
                id: xml_tag(seg, "instanceId")?,
                instance_type: xml_tag(seg, "instanceType").unwrap_or_default(),
                spot: xml_tag(seg, "instanceLifecycle").as_deref() == Some("spot"),
                zone: xml_tag(seg, "placement")
                    .and_then(|p| xml_tag(&p, "availabilityZone"))
                    .unwrap_or_default(),
                tile: tag_value(seg, "Purpose").as_deref() == Some("fcvm-fleet-tile"),
            })
        })
        .collect()
}

/// The value of an instance's tag `key` (its `tagSet`).
fn tag_value(instance: &str, key: &str) -> Option<String> {
    let tags = xml_tag(instance, "tagSet")?;
    set_items(&format!("<tagSet>{tags}</tagSet>"), "tagSet")
        .into_iter()
        .find(|item| xml_tag(item, "key").as_deref() == Some(key))
        .and_then(|item| xml_tag(item, "value"))
}

/// The prices of a DescribeSpotPriceHistory answer.
pub fn parse_spot_prices(xml: &str) -> Vec<SpotPrice> {
    set_items(xml, "spotPriceHistorySet")
        .into_iter()
        .filter_map(|seg| {
            Some((
                xml_tag(seg, "instanceType")?,
                xml_tag(seg, "availabilityZone")?,
                xml_tag(seg, "spotPrice")?
                    .parse::<f64>()
                    .ok()
                    .filter(|p| p.is_finite() && *p >= 0.0)?,
            ))
        })
        .collect()
}

/// The fleet's burn rate: USD per hour of `instances`, by region, with `spot` the known spot
/// prices by region and type (per zone). Returns the rate and, when any instance could not be
/// priced exactly, why.
pub fn burn_rate(
    instances: &BTreeMap<String, Vec<Billed>>,
    spot: &HashMap<(String, String), BTreeMap<String, f64>>,
    on_demand: &BTreeMap<String, f64>,
) -> (f64, Option<String>) {
    let (hosts, tiles, why) = burn_rates(instances, spot, on_demand);
    (hosts + tiles, why)
}

/// [`burn_rate`] split into the GPU hosts' and the EC2 tiles' parts.
pub fn burn_rates(
    instances: &BTreeMap<String, Vec<Billed>>,
    spot: &HashMap<(String, String), BTreeMap<String, f64>>,
    on_demand: &BTreeMap<String, f64>,
) -> (f64, f64, Option<String>) {
    let highest = on_demand.values().copied().fold(0.0, f64::max);
    let mut hosts = 0.0;
    let mut tiles = 0.0;
    let mut unpriced: Vec<String> = Vec::new();
    for (region, list) in instances {
        for b in list {
            let od = on_demand.get(&b.instance_type).copied();
            let price = if b.spot {
                spot.get(&(region.clone(), b.instance_type.clone()))
                    .and_then(|zones| {
                        zones
                            .get(&b.zone)
                            .copied()
                            .or_else(|| zones.values().copied().reduce(f64::max))
                    })
            } else {
                od
            };
            let p = match price {
                Some(p) => p,
                None => {
                    let market = if b.spot { "spot" } else { "on-demand" };
                    unpriced.push(format!("{} {market} in {region}", b.instance_type));
                    od.unwrap_or(highest)
                }
            };
            if b.tile {
                tiles += p;
            } else {
                hosts += p;
            }
        }
    }
    unpriced.sort();
    unpriced.dedup();
    let why = (!unpriced.is_empty()).then(|| {
        format!(
            "no price for {}; charged at the on-demand or highest rate",
            unpriced.join(", ")
        )
    });
    (hosts, tiles, why)
}

/// One pass's report: the burn rate, the instances it prices and why any could not be
/// priced exactly. A region in `stale` (no listing within [`STALE_AFTER`], see
/// [`stale_regions`]) may be running instances its last list does not show, so the rate is
/// at least `cap`, the fleet's launch cap: an inventory that stays unknown makes the fleet
/// rest, rather than run on an old list or, on a first start, on no rate at all. Before that
/// bound, an error while any region has never been listed: an unknown inventory is not an
/// empty one, so no rate is reported and the ledger keeps charging the last rate it has (the
/// one stored across a restart included).
pub fn burn_report(
    regions: &[String],
    listed: &BTreeMap<String, Vec<Billed>>,
    stale: &[String],
    spot: &HashMap<(String, String), BTreeMap<String, f64>>,
    on_demand: &BTreeMap<String, f64>,
    cap: f64,
    // What the listings do not show (the Lambda tiles): added to the listed rate, and so
    // inside the launch cap, which already counts it.
    extra: f64,
) -> std::result::Result<(f64, u32, Option<String>), String> {
    if !stale.is_empty() {
        let (rate, why) = burn_rate(listed, spot, on_demand);
        let rate = rate + extra;
        let instances = listed.values().map(|l| l.len() as u32).sum();
        let mut reason = format!(
            "no listing of {} for {} s; charged at the fleet's launch cap",
            stale.join(", "),
            STALE_AFTER.as_secs()
        );
        if let Some(w) = why {
            reason = format!("{reason}; {w}");
        }
        return Ok((rate.max(cap), instances, Some(reason)));
    }
    let unlisted: Vec<&str> = regions
        .iter()
        .filter(|r| !listed.contains_key(*r))
        .map(String::as_str)
        .collect();
    if !unlisted.is_empty() {
        return Err(format!(
            "no listing yet of {}; the last reported burn stays in use",
            unlisted.join(", ")
        ));
    }
    let (rate, why) = burn_rate(listed, spot, on_demand);
    let instances = listed.values().map(|l| l.len() as u32).sum();
    Ok((rate + extra, instances, why))
}

/// The meter `--daily-spend-usd` asks for, or `None` without it. A ceiling needs a fleet that
/// sleeps and instances to meter.
pub fn meter(args: &crate::cli::FleetCoordArgs) -> Result<Option<MeterConfig>> {
    let Some(usd) = args.daily_spend_usd else {
        return Ok(None);
    };
    anyhow::ensure!(
        usd.is_finite() && usd > 0.0,
        "--daily-spend-usd {usd} is not a positive number of USD"
    );
    anyhow::ensure!(
        super::power_managed(args),
        "--daily-spend-usd sleeps the fleet at the ceiling, so it needs --host-template or --sleep-when-idle"
    );
    let mut regions: Vec<String> = args
        .host_templates
        .iter()
        .map(|t| t.region.clone())
        .collect();
    if args.ec2 == crate::cli::FleetEc2::Aws {
        regions.extend(args.ec2_regions.iter().cloned());
    }
    regions.sort();
    regions.dedup();
    anyhow::ensure!(
        !regions.is_empty(),
        "--daily-spend-usd meters the instances the coordinator launches: it needs --host-template or --ec2 aws"
    );
    let on_demand = on_demand_rates(&args.spend_rates)?;
    Ok(Some(MeterConfig {
        driver: Arc::new(AwsMeter::new(args.fleet_name.clone())?),
        regions,
        cap_usd_per_hour: launch_cap_usd_per_hour(args, &on_demand),
        lambda_usd_per_instance_hour: (args.lambda == crate::cli::FleetLambda::Aws)
            .then(|| lambda_instance_rate(&on_demand)),
        on_demand,
        every: Duration::from_secs(30),
        spot_ttl: Duration::from_secs(1800),
        stale_after: STALE_AFTER,
    }))
}

/// List, price and report, every `cfg.every`, until the actor is gone.
pub async fn run(cfg: MeterConfig, handle: Handle) {
    let mut listed: BTreeMap<String, Vec<Billed>> = BTreeMap::new();
    let mut listed_at: HashMap<String, Instant> = HashMap::new();
    let started = Instant::now();
    let mut spot: HashMap<(String, String), BTreeMap<String, f64>> = HashMap::new();
    let mut spot_read: HashMap<(String, String), Instant> = HashMap::new();
    let mut degraded_now = false;
    let mut lambda_seen: HashMap<String, Instant> = HashMap::new();
    let mut every = tokio::time::interval(cfg.every);
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        every.tick().await;
        let mut problems: Vec<String> = Vec::new();
        for region in &cfg.regions {
            match cfg.driver.instances(region).await {
                Ok(list) => {
                    listed.insert(region.clone(), list);
                    listed_at.insert(region.clone(), Instant::now());
                }
                // Its last listed instances stay charged, up to `stale_after`.
                Err(e) => problems.push(format!("listing {region}: {e:#}")),
            }
        }
        // The spot prices of the types running now that are not cached or are stale.
        let now = Instant::now();
        for (region, list) in &listed {
            let mut types: Vec<String> = list
                .iter()
                .filter(|b| b.spot)
                .map(|b| b.instance_type.clone())
                .filter(|t| {
                    spot_read
                        .get(&(region.clone(), t.clone()))
                        .is_none_or(|at| now.duration_since(*at) >= cfg.spot_ttl)
                })
                .collect();
            types.sort();
            types.dedup();
            if types.is_empty() {
                continue;
            }
            match cfg.driver.spot_prices(region, &types).await {
                Ok(prices) => {
                    for t in &types {
                        let key = (region.clone(), t.clone());
                        let zones: BTreeMap<String, f64> = prices
                            .iter()
                            .filter(|(pt, _, _)| pt == t)
                            .fold(BTreeMap::new(), |mut m, (_, z, p)| {
                                let e = m.entry(z.clone()).or_insert(*p);
                                *e = e.max(*p);
                                m
                            });
                        // A type with no price answered stays unpriced and is asked again
                        // next pass; one answered is kept for the cache's life.
                        if !zones.is_empty() {
                            spot.insert(key.clone(), zones);
                            spot_read.insert(key, now);
                        }
                    }
                }
                // A stale price stays in use until a read succeeds.
                Err(e) => problems.push(format!("spot prices in {region}: {e:#}")),
            }
        }
        let stale = stale_regions(
            &cfg.regions,
            &listed_at,
            started,
            Instant::now(),
            cfg.stale_after,
        );
        // The Lambda tiles' instances are not listed: their charge is added to the listed
        // rate before the launch cap is applied, never on top of the cap.
        let lambda = cfg.lambda_usd_per_instance_hour.map(|rate| {
            let assigned: Vec<String> = handle
                .lambda
                .borrow()
                .iter()
                .map(|a| a.id.clone())
                .collect();
            lambda_burn(&assigned, &mut lambda_seen, Instant::now(), rate)
        });
        let burn = burn_report(
            &cfg.regions,
            &listed,
            &stale,
            &spot,
            &cfg.on_demand,
            cfg.cap_usd_per_hour,
            lambda.unwrap_or(0.0),
        );
        if handle.cmds.is_closed() {
            return;
        }
        match burn {
            Ok((usd_per_hour, instances, why)) => {
                problems.extend(why);
                let by_backing = match lambda {
                    // At the launch cap (an unknown inventory) the parts are not known.
                    Some(lambda) if stale.is_empty() => {
                        let (hosts, tiles, _) = burn_rates(&listed, &spot, &cfg.on_demand);
                        BTreeMap::from([
                            ("hosts".to_string(), hosts),
                            ("ec2".to_string(), tiles),
                            ("lambda".to_string(), lambda),
                        ])
                    }
                    _ => BTreeMap::new(),
                };
                tracing::debug!(usd_per_hour, instances, "spend meter");
                handle
                    .send(Event::Burn {
                        at: 0,
                        usd_per_hour,
                        instances,
                        by_backing,
                    })
                    .await;
            }
            Err(why) => problems.push(why),
        }
        let reason = (!problems.is_empty()).then(|| problems.join("; "));
        if reason.is_some() || degraded_now {
            if let Some(r) = &reason {
                tracing::warn!(reason = %r, "spend meter degraded");
            }
            degraded_now = reason.is_some();
            handle
                .send(Event::Degraded {
                    at: 0,
                    component: "spend".into(),
                    reason,
                })
                .await;
        }
    }
}

pub(super) const DRILL_DAY_KEY: &str = "drills/prod-budget-day";

fn drill_day(marker: Option<&[u8]>) -> Result<Option<u64>> {
    let Some(marker) = marker else {
        return Ok(None);
    };
    if marker == b"off" {
        return Ok(None);
    }
    let text = std::str::from_utf8(marker)?;
    anyhow::ensure!(text.len() == 10, "expected a UTC date or off");
    let date: jiff::civil::Date = text.parse().context("expected a UTC date or off")?;
    anyhow::ensure!(date.to_string() == text, "expected a UTC date or off");
    let seconds = date
        .to_zoned(jiff::tz::TimeZone::UTC)?
        .timestamp()
        .as_second();
    Ok(u64::try_from(seconds).ok().map(|s| s / 86_400))
}

/// Read the fixed production drill marker. `None` clears the override. An error leaves its
/// last observed value in the ledger.
pub async fn read_drill(obj: &super::store::S3Object) -> Result<Option<u64>> {
    let marker = tokio::time::timeout(Duration::from_secs(5), obj.drill_marker())
        .await
        .context("budget drill marker read timed out")??;
    drill_day(marker.as_deref())
}

/// Failed reads leave the last observed day in the durable spend ledger. The actor enforces
/// UTC expiry even if this task is offline at midnight.
pub(super) async fn watch_drill(obj: super::store::S3Object, handle: Handle) {
    let mut unavailable = false;
    let mut ticks = tokio::time::interval(Duration::from_secs(30));
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = handle.cmds.closed() => return,
            _ = ticks.tick() => {}
        }
        match read_drill(&obj).await {
            Ok(day) => {
                if unavailable {
                    tracing::info!("budget drill marker readable again");
                }
                unavailable = false;
                handle.send(Event::BudgetDrill { at: 0, day }).await;
            }
            Err(error) => {
                if !unavailable {
                    tracing::warn!(%error, "budget drill marker unavailable");
                }
                unavailable = true;
            }
        }
    }
}

/// The meter on AWS: EC2 query calls signed with the task's credentials.
pub struct AwsMeter {
    pub fleet: String,
    /// Per-region API base; default `https://ec2.<region>.amazonaws.com`.
    pub endpoints: BTreeMap<String, String>,
    chain: CredentialChain,
    http: reqwest::Client,
}

impl AwsMeter {
    pub fn new(fleet: String) -> Result<Self> {
        Ok(AwsMeter {
            fleet,
            endpoints: BTreeMap::new(),
            chain: CredentialChain::new()?,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .build()?,
        })
    }

    async fn call(&self, region: &str, params: &[(String, String)]) -> Result<String> {
        let mut body = String::from("Version=2016-11-15");
        for (k, v) in params {
            body.push('&');
            body.push_str(&uri_encode(k));
            body.push('=');
            body.push_str(&uri_encode(v));
        }
        let base = self
            .endpoints
            .get(region)
            .cloned()
            .unwrap_or_else(|| format!("https://ec2.{region}.amazonaws.com"));
        let (status, bytes) = signed(
            &self.chain,
            &self.http,
            &base,
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
        .await?;
        let xml = String::from_utf8_lossy(&bytes).to_string();
        anyhow::ensure!(
            status.is_success(),
            "EC2 in {region} answered {status}: {}: {}",
            xml_tag(&xml, "Code").unwrap_or_default(),
            xml_tag(&xml, "Message").unwrap_or_default()
        );
        Ok(xml)
    }

    /// Every page of one call.
    async fn pages(&self, region: &str, params: Vec<(String, String)>) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let mut next: Option<String> = None;
        loop {
            let mut p = params.clone();
            if let Some(t) = &next {
                p.push(("NextToken".into(), t.clone()));
            }
            let xml = self.call(region, &p).await?;
            next = xml_tag(&xml, "nextToken").filter(|t| !t.is_empty());
            out.push(xml);
            if next.is_none() || out.len() >= 50 {
                return Ok(out);
            }
        }
    }
}

fn p(k: &str, v: impl Into<String>) -> (String, String) {
    (k.into(), v.into())
}

#[async_trait]
impl MeterDriver for AwsMeter {
    async fn instances(&self, region: &str) -> Result<Vec<Billed>> {
        let mut params = vec![
            p("Action", "DescribeInstances"),
            p("Filter.1.Name", "tag:Fleet"),
            p("Filter.1.Value.1", self.fleet.as_str()),
            p("Filter.2.Name", "tag:Purpose"),
            p("Filter.2.Value.1", "fcvm-fleet-host"),
            p("Filter.2.Value.2", "fcvm-fleet-tile"),
            p("Filter.3.Name", "instance-state-name"),
        ];
        for (i, s) in BILLED_STATES.iter().enumerate() {
            params.push(p(&format!("Filter.3.Value.{}", i + 1), *s));
        }
        let mut out: Vec<Billed> = Vec::new();
        for xml in self.pages(region, params).await? {
            out.extend(parse_instances(&xml));
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out.dedup_by(|a, b| a.id == b.id);
        Ok(out)
    }

    async fn spot_prices(&self, region: &str, types: &[String]) -> Result<Vec<SpotPrice>> {
        // A start time of now answers the price in effect now, per zone.
        let now = jiff::Timestamp::now()
            .strftime("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        let mut params = vec![
            p("Action", "DescribeSpotPriceHistory"),
            p("StartTime", now.as_str()),
            p("ProductDescription.1", "Linux/UNIX"),
        ];
        for (i, t) in types.iter().enumerate() {
            params.push(p(&format!("InstanceType.{}", i + 1), t.as_str()));
        }
        let mut out = Vec::new();
        for xml in self.pages(region, params).await? {
            out.extend(parse_spot_prices(&xml));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drill_marker_accepts_only_a_complete_utc_date_or_off() {
        assert_eq!(drill_day(None).unwrap(), None);
        assert_eq!(drill_day(Some(b"off")).unwrap(), None);
        assert_eq!(drill_day(Some(b"1970-01-02")).unwrap(), Some(1));
        assert_eq!(drill_day(Some(b"2000-02-29")).unwrap(), Some(11016));
        for invalid in [
            b"2026-02-29".as_slice(),
            b"2026-10-10x",
            b"off\n",
            b"",
            &[0xff],
        ] {
            assert!(drill_day(Some(invalid)).is_err(), "{invalid:?}");
        }
    }

    const INSTANCES: &str = r#"<DescribeInstancesResponse><reservationSet><item><instancesSet>
<item><instanceId>i-host1</instanceId><instanceType>g4dn.metal</instanceType><placement><availabilityZone>us-east-1b</availabilityZone><tenancy>default</tenancy></placement><instanceState><code>16</code><name>running</name></instanceState><instanceLifecycle>spot</instanceLifecycle><tagSet><item><key>Fleet</key><value>stage</value></item></tagSet></item>
<item><availabilityZone>ignored</availabilityZone><instanceId>i-tile1</instanceId><placement><availabilityZone>us-east-1c</availabilityZone></placement><instanceType>g4dn.xlarge</instanceType><tagSet><item><key>Fleet</key><value>stage</value></item><item><key>Purpose</key><value>fcvm-fleet-tile</value></item></tagSet><instanceState><name>pending</name></instanceState></item>
</instancesSet></item></reservationSet></DescribeInstancesResponse>"#;

    const PRICES: &str = r#"<DescribeSpotPriceHistoryResponse><spotPriceHistorySet>
<item><instanceType>g4dn.metal</instanceType><productDescription>Linux/UNIX</productDescription><spotPrice>4.491800</spotPrice><timestamp>2026-11-19T05:00:00.000Z</timestamp><availabilityZone>us-east-1b</availabilityZone></item>
<item><instanceType>g4dn.metal</instanceType><productDescription>Linux/UNIX</productDescription><spotPrice>6.268000</spotPrice><timestamp>2026-11-19T04:00:00.000Z</timestamp><availabilityZone>us-east-1a</availabilityZone></item>
</spotPriceHistorySet><nextToken/></DescribeSpotPriceHistoryResponse>"#;

    fn host(id: &str, ty: &str, spot: bool, zone: &str) -> Billed {
        Billed {
            id: id.into(),
            instance_type: ty.into(),
            spot,
            zone: zone.into(),
            tile: false,
        }
    }

    #[test]
    fn describe_instances_names_type_market_and_zone() {
        assert_eq!(
            parse_instances(INSTANCES),
            vec![
                host("i-host1", "g4dn.metal", true, "us-east-1b"),
                Billed {
                    tile: true,
                    ..host("i-tile1", "g4dn.xlarge", false, "us-east-1c")
                },
            ]
        );
    }

    #[test]
    fn a_lambda_tile_is_charged_two_instances_until_its_delete_tail_ends() {
        let rate = lambda_instance_rate(&on_demand_rates(&[]).unwrap());
        assert!((rate - (0.526 + 0.051285)).abs() < 1e-9, "{rate}");
        // One tile: two g4dn.xlarge with the management fee each.
        let mut one_tile = HashMap::new();
        let charge = lambda_burn(&["lam-1".to_string()], &mut one_tile, Instant::now(), rate);
        assert!((charge - 1.15457).abs() < 1e-9, "{charge}");
        let mut seen = HashMap::new();
        let t0 = Instant::now();
        let both = vec!["lam-1".to_string(), "lam-2".to_string()];
        assert!((lambda_burn(&both, &mut seen, t0, rate) - 4.0 * rate).abs() < 1e-9);
        // lam-2 left the assignment: still charged within the tail, not after it.
        let one = vec!["lam-1".to_string()];
        let within = t0 + LAMBDA_DELETE_TAIL - Duration::from_secs(1);
        assert!((lambda_burn(&one, &mut seen, within, rate) - 4.0 * rate).abs() < 1e-9);
        let after = within + Duration::from_secs(2);
        assert!((lambda_burn(&one, &mut seen, after, rate) - 2.0 * rate).abs() < 1e-9);
        assert_eq!(
            lambda_burn(&[], &mut seen, after + LAMBDA_DELETE_TAIL, rate),
            0.0
        );
    }

    #[test]
    fn the_burn_splits_into_hosts_and_tiles() {
        let listed = BTreeMap::from([(
            "us-east-1".to_string(),
            vec![
                host("i-h", "g4dn.metal", false, "us-east-1a"),
                Billed {
                    tile: true,
                    ..host("i-t", "g4dn.xlarge", false, "us-east-1a")
                },
            ],
        )]);
        let (hosts, tiles, why) =
            burn_rates(&listed, &HashMap::new(), &on_demand_rates(&[]).unwrap());
        assert_eq!((hosts, tiles, why), (7.824, 0.526, None));
    }

    #[test]
    fn spot_price_history_gives_each_zones_price() {
        assert_eq!(
            parse_spot_prices(PRICES),
            vec![
                ("g4dn.metal".into(), "us-east-1b".into(), 4.4918),
                ("g4dn.metal".into(), "us-east-1a".into(), 6.268),
            ]
        );
    }

    #[test]
    fn spot_is_priced_at_its_zones_spot_price_and_on_demand_at_its_rate() {
        let on_demand = on_demand_rates(&[]).unwrap();
        let instances = BTreeMap::from([("us-east-1".to_string(), parse_instances(INSTANCES))]);
        let spot = HashMap::from([(
            ("us-east-1".to_string(), "g4dn.metal".to_string()),
            BTreeMap::from([
                ("us-east-1a".to_string(), 6.268),
                ("us-east-1b".to_string(), 4.4918),
            ]),
        )]);
        let (rate, why) = burn_rate(&instances, &spot, &on_demand);
        assert!((rate - (4.4918 + 0.526)).abs() < 1e-9, "{rate}");
        assert_eq!(why, None);
    }

    #[test]
    fn what_cannot_be_priced_is_charged_high_and_said() {
        let on_demand = on_demand_rates(&["g4dn.metal=8".to_string()]).unwrap();
        let instances = BTreeMap::from([(
            "us-east-2".to_string(),
            vec![
                // No spot price known: its on-demand rate.
                host("i-1", "g4dn.metal", true, "us-east-2a"),
                // A spot price known for another zone only: the highest zone's.
                host("i-2", "g5g.metal", true, "us-east-2c"),
                // No rate for its type: the highest rate known.
                host("i-3", "p5.48xlarge", false, "us-east-2a"),
            ],
        )]);
        let spot = HashMap::from([(
            ("us-east-2".to_string(), "g5g.metal".to_string()),
            BTreeMap::from([
                ("us-east-2a".to_string(), 0.5),
                ("us-east-2b".to_string(), 0.6),
            ]),
        )]);
        let (rate, why) = burn_rate(&instances, &spot, &on_demand);
        assert!((rate - (8.0 + 0.6 + 8.0)).abs() < 1e-9, "{rate}");
        assert_eq!(
            why.as_deref(),
            Some(
                "no price for g4dn.metal spot in us-east-2, p5.48xlarge on-demand in us-east-2; \
                 charged at the on-demand or highest rate"
            )
        );
    }

    #[test]
    fn spend_rates_override_the_defaults_and_refuse_nonsense() {
        let r = on_demand_rates(&["g4dn.metal=9.5".into(), "g6e.48xlarge=30.1".into()]).unwrap();
        assert_eq!(r["g4dn.metal"], 9.5);
        assert_eq!(r["g6e.48xlarge"], 30.1);
        assert_eq!(r["g5g.metal"], 2.744);
        for bad in [
            "g4dn.metal",
            "=1",
            "g4dn.metal=-1",
            "g4dn.metal=nan",
            "g4dn.metal=x",
        ] {
            assert!(on_demand_rates(&[bad.into()]).is_err(), "{bad}");
        }
    }

    #[test]
    fn no_rate_is_reported_until_every_region_has_been_listed() {
        let on_demand = on_demand_rates(&[]).unwrap();
        let regions = vec!["us-east-1".to_string(), "us-east-2".to_string()];
        let spot = HashMap::new();
        // A restart whose first listings failed: reporting 0 would overwrite the stored rate.
        let none = BTreeMap::new();
        assert_eq!(
            burn_report(&regions, &none, &[], &spot, &on_demand, 0.0, 0.0),
            Err(
                "no listing yet of us-east-1, us-east-2; the last reported burn stays in use"
                    .into()
            )
        );
        // One region listed, the other not yet: its hosts would go uncharged.
        let mut listed = BTreeMap::from([(
            "us-east-1".to_string(),
            vec![host("i-1", "g4dn.xlarge", false, "us-east-1a")],
        )]);
        assert!(burn_report(&regions, &listed, &[], &spot, &on_demand, 0.0, 0.0).is_err());
        listed.insert("us-east-2".to_string(), Vec::new());
        assert_eq!(
            burn_report(&regions, &listed, &[], &spot, &on_demand, 0.0, 0.0),
            Ok((0.526, 1, None))
        );
    }

    #[test]
    fn a_region_unlisted_past_the_bound_is_charged_at_the_launch_cap() {
        let on_demand = on_demand_rates(&[]).unwrap();
        let regions = vec!["us-east-1".to_string(), "us-east-2".to_string()];
        let spot = HashMap::new();
        let cap = 100.0;
        // Listed once, then its listings kept failing: the old list is not trusted past the bound.
        let listed = BTreeMap::from([
            (
                "us-east-1".to_string(),
                vec![host("i-1", "g4dn.xlarge", false, "us-east-1a")],
            ),
            ("us-east-2".to_string(), Vec::new()),
        ]);
        let stale = vec!["us-east-1".to_string()];
        let (rate, instances, why) =
            burn_report(&regions, &listed, &stale, &spot, &on_demand, cap, 0.0).unwrap();
        assert_eq!((rate, instances), (cap, 1));
        assert!(why.unwrap().contains("us-east-1"));
        // Never listed at all, past the bound: no longer "keep the last rate", which on a first
        // start is none.
        let stale = regions.clone();
        let none = BTreeMap::new();
        let (rate, _, _) =
            burn_report(&regions, &none, &stale, &spot, &on_demand, cap, 0.0).unwrap();
        assert_eq!(rate, cap);
        // The Lambda tiles' charge is inside the cap, which already counts them: an unknown
        // inventory is charged the cap, not the cap and the Lambda tiles again.
        let (rate, _, _) =
            burn_report(&regions, &none, &stale, &spot, &on_demand, cap, 2.0).unwrap();
        assert_eq!(rate, cap);
        // A known inventory adds them to what it lists.
        let fresh = BTreeMap::from([
            ("us-east-1".to_string(), Vec::new()),
            ("us-east-2".to_string(), Vec::new()),
        ]);
        let (rate, _, _) = burn_report(&regions, &fresh, &[], &spot, &on_demand, cap, 2.0).unwrap();
        assert_eq!(rate, 2.0);
        // A known rate above the cap is kept.
        let big = BTreeMap::from([
            (
                "us-east-1".to_string(),
                vec![host("i-1", "g4dn.metal", false, "us-east-1a"); 40],
            ),
            ("us-east-2".to_string(), Vec::new()),
        ]);
        let stale = vec!["us-east-2".to_string()];
        let (rate, _, _) =
            burn_report(&regions, &big, &stale, &spot, &on_demand, cap, 0.0).unwrap();
        assert!(rate > cap);
    }

    #[test]
    fn regions_go_stale_after_the_bound_from_their_last_listing_or_the_start() {
        let regions = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let start = Instant::now();
        let bound = Duration::from_secs(300);
        let listed_at = HashMap::from([
            ("a".to_string(), start + Duration::from_secs(100)),
            ("b".to_string(), start + Duration::from_secs(390)),
        ]);
        let at = |s| start + Duration::from_secs(s);
        // At 399 s: a was last listed 299 s ago, b 9 s ago, c never (399 s since the start).
        assert_eq!(
            stale_regions(&regions, &listed_at, start, at(399), bound),
            ["c"]
        );
        // At 400 s: a reaches the bound too.
        assert_eq!(
            stale_regions(&regions, &listed_at, start, at(400), bound),
            ["a", "c"]
        );
        // Inside the grace after a start, nothing is stale yet.
        assert!(stale_regions(&regions, &HashMap::new(), start, at(60), bound).is_empty());
    }

    #[test]
    fn the_launch_cap_is_every_host_and_tile_the_fleet_may_run_at_the_highest_rate() {
        let args = coord(&[
            "--host-template",
            "us-east-1:fcvm-fleet-host-x86_64",
            "--host-subnet",
            "us-east-1:subnet-0123456789abcdef0",
            "--awake-hosts",
            "2",
            "--ec2",
            "aws",
            "--ec2-regions",
            "us-east-1",
            "--ec2-tiles",
            "8",
            "--daily-spend-usd",
            "750",
            "--fleet-name",
            "prod",
        ]);
        let m = meter(&args).unwrap().unwrap();
        let highest = on_demand_rates(&[])
            .unwrap()
            .values()
            .copied()
            .fold(0.0, f64::max);
        // One host region: two awake hosts and a replacement. Eight tiles, each with a relaunch
        // in flight.
        assert_eq!(m.cap_usd_per_hour, highest * (3 + 16) as f64);
        assert_eq!(m.stale_after, STALE_AFTER);
    }

    #[test]
    fn lambda_tiles_come_out_of_the_ec2_tiles_in_the_launch_cap() {
        let args = coord(&[
            "--host-template",
            "us-east-1:fcvm-fleet-host-x86_64",
            "--host-subnet",
            "us-east-1:subnet-0123456789abcdef0",
            "--awake-hosts",
            "2",
            "--ec2",
            "aws",
            "--ec2-regions",
            "us-east-1",
            "--ec2-tiles",
            "8",
            "--lambda",
            "aws",
            "--lambda-tiles",
            "4",
            "--daily-spend-usd",
            "750",
            "--fleet-name",
            "prod",
        ]);
        let m = meter(&args).unwrap().unwrap();
        let rates = on_demand_rates(&[]).unwrap();
        let highest = rates.values().copied().fold(0.0, f64::max);
        let lane = lambda_instance_rate(&rates);
        // Hosts as before; 4 EC2 tiles with a relaunch each; 4 Lambda tiles with their two
        // instances and a third while a race's version comes up.
        let want = highest * (3 + 8) as f64 + 4.0 * 3.0 * lane;
        assert!(
            (m.cap_usd_per_hour - want).abs() < 1e-9,
            "{} != {want}",
            m.cap_usd_per_hour
        );
        assert_eq!(m.lambda_usd_per_instance_hour, Some(lane));
    }

    fn coord(extra: &[&str]) -> crate::cli::FleetCoordArgs {
        use crate::cli::{Cli, Commands, FleetCommand};
        use clap::Parser;
        let mut argv = vec!["fcvm", "fleet", "coord", "--listen", "127.0.0.1:0"];
        argv.extend_from_slice(extra);
        match Cli::try_parse_from(argv).expect("parses").cmd {
            Commands::Fleet(f) => match f.cmd {
                FleetCommand::Coord(a) => *a,
                _ => panic!("coord"),
            },
            _ => panic!("fleet"),
        }
    }

    #[test]
    fn a_ceiling_needs_a_fleet_that_sleeps_and_instances_to_meter() {
        assert!(meter(&coord(&[])).unwrap().is_none());
        let hosts = [
            "--host-template",
            "us-east-1:fcvm-fleet-host-x86_64",
            "--host-subnet",
            "us-east-1:subnet-0123456789abcdef0",
        ];
        let mut args = hosts.to_vec();
        args.extend(["--ec2", "aws", "--ec2-regions", "us-east-2,us-east-1"]);
        args.extend(["--daily-spend-usd", "750", "--fleet-name", "prod"]);
        let m = meter(&coord(&args)).unwrap().unwrap();
        assert_eq!(m.regions, ["us-east-1", "us-east-2"]);
        // Nothing to sleep, nothing to meter, or not a positive amount: refused at start.
        for bad in [
            vec![
                "--daily-spend-usd",
                "750",
                "--ec2",
                "aws",
                "--ec2-regions",
                "us-east-1",
            ],
            vec!["--daily-spend-usd", "750", "--sleep-when-idle"],
            vec![
                "--daily-spend-usd",
                "0",
                "--sleep-when-idle",
                "--ec2",
                "aws",
                "--ec2-regions",
                "us-east-1",
            ],
        ] {
            assert!(meter(&coord(&bad)).is_err(), "{bad:?}");
        }
    }
}
