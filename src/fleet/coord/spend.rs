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
//! listed instances. Each of those marks the `spend` component degraded.
//!
//! Each pass also writes one CloudWatch embedded metric format line to stdout (the log
//! stream; tracing writes to stderr): `SpendUsdToday` and `Power` (0 asleep, 1 waking,
//! 2 awake) in namespace [`EMF_NAMESPACE`], with a `Fleet` dimension, from the snapshot the
//! previous report produced.

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

pub const EMF_NAMESPACE: &str = "FCVM/Fleet";

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
    /// The `Fleet` dimension of the metrics.
    pub fleet: String,
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
            })
        })
        .collect()
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
    let highest = on_demand.values().copied().fold(0.0, f64::max);
    let mut total = 0.0;
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
            total += match price {
                Some(p) => p,
                None => {
                    let market = if b.spot { "spot" } else { "on-demand" };
                    unpriced.push(format!("{} {market} in {region}", b.instance_type));
                    od.unwrap_or(highest)
                }
            };
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
    (total, why)
}

/// One pass's report: the burn rate, the instances it prices and why any could not be
/// priced exactly. An error while any region has never been listed: an unknown inventory is
/// not an empty one, so no rate is reported and the ledger keeps charging the last rate it
/// has (the one stored across a restart included).
pub fn burn_report(
    regions: &[String],
    listed: &BTreeMap<String, Vec<Billed>>,
    spot: &HashMap<(String, String), BTreeMap<String, f64>>,
    on_demand: &BTreeMap<String, f64>,
) -> std::result::Result<(f64, u32, Option<String>), String> {
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
    Ok((rate, instances, why))
}

/// The metric line for one pass, from a snapshot's text; `None` before the first snapshot or
/// without a ceiling.
pub fn emf_line(snapshot: &str, fleet: &str, now_ms: u64) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(snapshot).ok()?;
    let f = &v["state"]["fleet"];
    let usd = f["spend"]["usd_today"].as_f64()?;
    let power = match f["power"]["state"].as_str()? {
        "asleep" => 0,
        "waking" => 1,
        _ => 2,
    };
    Some(
        serde_json::json!({
            "_aws": {
                "Timestamp": now_ms,
                "CloudWatchMetrics": [{
                    "Namespace": EMF_NAMESPACE,
                    "Dimensions": [["Fleet"]],
                    "Metrics": [
                        {"Name": "SpendUsdToday", "Unit": "None"},
                        {"Name": "Power", "Unit": "None"},
                    ],
                }],
            },
            "Fleet": fleet,
            "SpendUsdToday": usd,
            "Power": power,
        })
        .to_string(),
    )
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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
    Ok(Some(MeterConfig {
        driver: Arc::new(AwsMeter::new(args.fleet_name.clone())?),
        regions,
        on_demand: on_demand_rates(&args.spend_rates)?,
        every: Duration::from_secs(30),
        spot_ttl: Duration::from_secs(1800),
        fleet: args.fleet_name.clone(),
    }))
}

/// List, price and report, every `cfg.every`, until the actor is gone.
pub async fn run(cfg: MeterConfig, handle: Handle) {
    let mut listed: BTreeMap<String, Vec<Billed>> = BTreeMap::new();
    let mut spot: HashMap<(String, String), BTreeMap<String, f64>> = HashMap::new();
    let mut spot_read: HashMap<(String, String), Instant> = HashMap::new();
    let mut degraded_now = false;
    let mut every = tokio::time::interval(cfg.every);
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        every.tick().await;
        if let Some(line) = emf_line(&handle.snapshot.borrow().text, &cfg.fleet, unix_ms()) {
            println!("{line}");
        }
        let mut problems: Vec<String> = Vec::new();
        for region in &cfg.regions {
            match cfg.driver.instances(region).await {
                Ok(list) => {
                    listed.insert(region.clone(), list);
                }
                // Its last listed instances stay charged.
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
        let burn = burn_report(&cfg.regions, &listed, &spot, &cfg.on_demand);
        if handle.cmds.is_closed() {
            return;
        }
        match burn {
            Ok((usd_per_hour, instances, why)) => {
                problems.extend(why);
                tracing::debug!(usd_per_hour, instances, "spend meter");
                handle
                    .send(Event::Burn {
                        at: 0,
                        usd_per_hour,
                        instances,
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

    const INSTANCES: &str = r#"<DescribeInstancesResponse><reservationSet><item><instancesSet>
<item><instanceId>i-host1</instanceId><instanceType>g4dn.metal</instanceType><placement><availabilityZone>us-east-1b</availabilityZone><tenancy>default</tenancy></placement><instanceState><code>16</code><name>running</name></instanceState><instanceLifecycle>spot</instanceLifecycle><tagSet><item><key>Fleet</key><value>stage</value></item></tagSet></item>
<item><availabilityZone>ignored</availabilityZone><instanceId>i-tile1</instanceId><placement><availabilityZone>us-east-1c</availabilityZone></placement><instanceType>g4dn.xlarge</instanceType><instanceState><name>pending</name></instanceState></item>
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
        }
    }

    #[test]
    fn describe_instances_names_type_market_and_zone() {
        assert_eq!(
            parse_instances(INSTANCES),
            vec![
                host("i-host1", "g4dn.metal", true, "us-east-1b"),
                host("i-tile1", "g4dn.xlarge", false, "us-east-1c"),
            ]
        );
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
            burn_report(&regions, &none, &spot, &on_demand),
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
        assert!(burn_report(&regions, &listed, &spot, &on_demand).is_err());
        listed.insert("us-east-2".to_string(), Vec::new());
        assert_eq!(
            burn_report(&regions, &listed, &spot, &on_demand),
            Ok((0.526, 1, None))
        );
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
        assert_eq!(m.fleet, "prod");
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

    #[test]
    fn the_metric_line_carries_spend_and_power_with_the_fleet_dimension() {
        let snap = r#"{"type":"snapshot","version":3,"state":{"fleet":{"power":{"state":"waking"},"spend":{"usd_today":12.34}}}}"#;
        let line: serde_json::Value =
            serde_json::from_str(&emf_line(snap, "prod", 1_700_000_000_000).unwrap()).unwrap();
        assert_eq!(
            line,
            serde_json::json!({
                "_aws": {
                    "Timestamp": 1_700_000_000_000u64,
                    "CloudWatchMetrics": [{
                        "Namespace": "FCVM/Fleet",
                        "Dimensions": [["Fleet"]],
                        "Metrics": [
                            {"Name": "SpendUsdToday", "Unit": "None"},
                            {"Name": "Power", "Unit": "None"},
                        ],
                    }],
                },
                "Fleet": "prod",
                "SpendUsdToday": 12.34,
                "Power": 1,
            })
        );
        // No snapshot yet, or no ceiling: no line.
        assert_eq!(emf_line("", "prod", 0), None);
        let no_ceiling =
            r#"{"type":"snapshot","version":3,"state":{"fleet":{"power":{"state":"awake"}}}}"#;
        assert_eq!(emf_line(no_ceiling, "prod", 0), None);
    }
}
