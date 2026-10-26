//! The EC2 half on AWS: one instance per tile from the tile launch template (one-time spot),
//! tagged at launch (`Purpose=fcvm-fleet-tile`, `FleetTile=<tile>`), terminated by id. A
//! launch tries each fleet region in order, and in a region with configured subnets each
//! subnet (one per AZ) in order, before the next region; when every one refuses for capacity
//! it tries the on-demand template the same way, if one is configured, and otherwise reports
//! no capacity, which the tile records as a capacity wait.
//!
//! The pool (`pool_max` above 0, with `pool_template`): a tile that leaves the assignment
//! (the fleet sleeps) has its instance stopped and tagged `FleetPool=<fleet>` instead of
//! terminated, while fewer than `pool_max` instances are pooled, and a tile with no instance
//! (the next wake) starts a pooled one before it launches. A stopped tile starts to its first
//! frame in 15.5 s, against 81 to 84 s for a launch (g4dn.xlarge, staging, 2026-10-25), since
//! its volume is already read from its snapshot and its first boot is done. Cold launches use
//! `pool_template` (persistent spot that stops on interruption, so the instance can be
//! stopped) in place of the one-time spot template. Every terminate cancels the instance's
//! spot request first, or a persistent request would launch a replacement. A pooled
//! instance whose image is not the pool template's current one is terminated, not started.
//! A relaunch of a running tile (a race) never uses the pool.
//!
//! The tile serves its renderer over TLS on 443 with the fleet's wildcard certificate, as
//! `<tile>.<zone>` (the coordinator's DNS writes that name). `ready` waits for the instance to
//! run, then for `/health` there, resolving the name to the instance's address itself so
//! readiness does not wait for DNS. `alive` reads the instance's state and asks the same
//! `/health` again, so a renderer that crashed on a running instance is noticed.

use super::aws::{set_items, signed, xml_tag, CredentialChain};
use super::ec2::{Ec2Driver, LaunchError, Launched, Liveness};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use fleet_core::protocol::{BudgetBody, RendererScene};
use std::collections::{BTreeMap, HashSet};
use std::time::{Duration, Instant};

pub struct AwsEc2 {
    pub regions: Vec<String>,
    pub spot_template: String,
    pub on_demand_template: Option<String>,
    /// Tiles are `<tile>.<zone>`.
    pub zone: String,
    /// Per-region API base; default `https://ec2.<region>.amazonaws.com`.
    pub endpoints: BTreeMap<String, String>,
    /// How the renderer's health is reached (`https` on 443; tests use plain HTTP).
    pub health_scheme: String,
    pub health_port: u16,
    pub ready_timeout: Duration,
    /// How often a starting instance is described (its state and addresses).
    pub poll: Duration,
    /// How often its renderer's `/health` is asked once the instance runs with an address.
    pub health_poll: Duration,
    /// This fleet's name, tagged on every tile at launch (`Fleet=<name>`): the sweep
    /// terminates only this fleet's tiles, not another coordinator's in the same account.
    pub fleet: String,
    /// Stopped instances kept for the next wake, at most this many (0: no pool).
    pub pool_max: usize,
    /// The template cold launches use while the pool is on (an instance that can be stopped).
    pub pool_template: Option<String>,
    /// Pooled instances this coordinator has started and not released since.
    claimed: std::sync::Mutex<HashSet<String>>,
    /// Held while a release counts the pool and tags an instance into it.
    pool_lock: tokio::sync::Mutex<()>,
    /// Per region, the subnets (one per AZ) a launch tries in order, and the security group
    /// the tile's interface gets there. A region without subnets launches in the template's.
    pub networks: BTreeMap<String, Network>,
    chain: CredentialChain,
    http: reqwest::Client,
}

/// One instance of a DescribeInstances answer.
#[derive(Debug, Clone, PartialEq)]
pub struct Instance {
    pub id: String,
    pub state: String,
    pub image: Option<String>,
    pub spot_request: Option<String>,
    /// Tagged `FleetPool`.
    pub pooled: bool,
    /// When it was launched (Unix seconds), if the answer says.
    pub launched_at: Option<i64>,
}

/// A pooled instance launched longer ago than this is terminated, not started: a
/// persistent spot request launched without `ValidUntil` expires after 7 days, and its
/// stopped instance cannot be started after that.
pub const POOL_MAX_AGE: Duration = Duration::from_secs(6 * 24 * 3600);

/// The persistent requests of a DescribeSpotInstanceRequests answer, one per item of its
/// `spotInstanceRequestSet`.
pub fn persistent_requests(xml: &str) -> Vec<String> {
    set_items(xml, "spotInstanceRequestSet")
        .into_iter()
        .filter(|item| xml_tag(item, "type").as_deref() == Some("persistent"))
        .filter_map(|item| xml_tag(item, "spotInstanceRequestId"))
        .collect()
}

/// The instances of a DescribeInstances answer, one per item of its `instancesSet` lists:
/// an instance's fields can come in any order, so the item bounds it.
pub fn parse_instances(xml: &str) -> Vec<Instance> {
    set_items(xml, "instancesSet")
        .into_iter()
        .filter_map(|seg| {
            Some(Instance {
                id: xml_tag(seg, "instanceId")?,
                state: xml_tag(seg, "instanceState")
                    .and_then(|s| xml_tag(&s, "name"))
                    .unwrap_or_default(),
                image: xml_tag(seg, "imageId"),
                spot_request: xml_tag(seg, "spotInstanceRequestId").filter(|r| !r.is_empty()),
                pooled: seg.contains("<key>FleetPool</key>"),
                launched_at: xml_tag(seg, "launchTime")
                    .and_then(|t| chrono::DateTime::parse_from_rfc3339(&t).ok())
                    .map(|t| t.timestamp()),
            })
        })
        .collect()
}

/// A region's tile network: its subnets, tried in order, and the security group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Network {
    pub subnets: Vec<String>,
    pub security_group: String,
}

/// The tile networks from `--ec2-subnets` (`<region>:<subnet>[,<subnet>...]`) and
/// `--ec2-security-group` (`<region>:<group>`). Every region with subnets needs a group;
/// empty entries (an empty environment variable) are skipped.
pub fn networks(subnets: &[String], groups: &[String]) -> Result<BTreeMap<String, Network>> {
    let split = |flag: &str, entry: &str| -> Result<(String, String)> {
        let (region, rest) = entry
            .split_once(':')
            .filter(|(r, v)| !r.is_empty() && !v.is_empty())
            .with_context(|| format!("{flag} {entry:?} is not <region>:<value>"))?;
        Ok((region.to_string(), rest.to_string()))
    };
    let mut sgs = BTreeMap::new();
    for g in groups.iter().map(|g| g.trim()).filter(|g| !g.is_empty()) {
        let (region, sg) = split("--ec2-security-group", g)?;
        anyhow::ensure!(
            sgs.insert(region.clone(), sg).is_none(),
            "--ec2-security-group names {region} twice"
        );
    }
    let mut out = BTreeMap::new();
    for e in subnets.iter().map(|e| e.trim()).filter(|e| !e.is_empty()) {
        let (region, list) = split("--ec2-subnets", e)?;
        let subnets: Vec<String> = list
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        anyhow::ensure!(!subnets.is_empty(), "--ec2-subnets {e:?} names no subnet");
        let security_group = sgs.get(&region).cloned().with_context(|| {
            format!("--ec2-subnets names {region}, which has no --ec2-security-group")
        })?;
        anyhow::ensure!(
            out.insert(
                region.clone(),
                Network {
                    subnets,
                    security_group
                }
            )
            .is_none(),
            "--ec2-subnets names {region} twice"
        );
    }
    Ok(out)
}

/// Errors EC2 gives when it has no capacity for the request (as opposed to a broken one).
const CAPACITY_CODES: &[&str] = &[
    "InsufficientInstanceCapacity",
    "InsufficientCapacity",
    "InsufficientHostCapacity",
    "InsufficientReservedInstanceCapacity",
    "SpotMaxPriceTooLow",
    "MaxSpotInstanceCountExceeded",
    "Unsupported",
];

/// How often a starting tile's renderer is asked for `/health`.
pub const HEALTH_POLL: Duration = Duration::from_millis(250);

enum CallError {
    Capacity(String),
    Other(anyhow::Error),
}

/// What the renderer's `/health` said. It answers 200 while CUDA starts and after it has
/// failed: ready means `ready` is true, and an `error` is a renderer that will not render.
enum Health {
    Ready,
    NotReady,
    Failed(String),
    /// No answer, or not one this code can read.
    Silent(String),
}

impl AwsEc2 {
    /// A client that reaches the tile's renderer by its address under its DNS name (the
    /// name its certificate is for), and the URL of `path` on it.
    fn renderer(
        &self,
        tile: &str,
        launched: &Launched,
        path: &str,
    ) -> Result<(reqwest::Client, String)> {
        let ip = launched
            .ipv4
            .as_deref()
            .context("the tile has no address yet")?;
        self.renderer_at(tile, ip, path)
    }

    /// Ask the tile's renderer at `ip` for its `/health`.
    async fn probe(&self, tile: &str, ip: &str) -> Health {
        let silent = |e: &dyn std::fmt::Display| Health::Silent(format!("{e:#}"));
        let (client, url) = match self.renderer_at(tile, ip, "/health") {
            Ok(c) => c,
            Err(e) => return silent(&e),
        };
        let r = match client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => return silent(&e),
        };
        if !r.status().is_success() {
            return Health::Silent(format!("GET {url} answered {}", r.status()));
        }
        let h = match r.json::<serde_json::Value>().await {
            Ok(h) => h,
            Err(e) => return silent(&e),
        };
        if let Some(e) = h.get("error").filter(|e| !e.is_null()) {
            return Health::Failed(e.to_string());
        }
        if h["ready"] == serde_json::Value::Bool(true) {
            Health::Ready
        } else {
            Health::NotReady
        }
    }

    fn renderer_at(&self, tile: &str, ip: &str, path: &str) -> Result<(reqwest::Client, String)> {
        let addr: std::net::IpAddr = ip.parse().context("the tile's address")?;
        let name = format!("{tile}.{}", self.zone);
        let client = reqwest::Client::builder()
            .resolve(&name, std::net::SocketAddr::new(addr, self.health_port))
            .timeout(Duration::from_secs(5))
            .build()?;
        let url = format!("{}://{name}:{}{path}", self.health_scheme, self.health_port);
        Ok((client, url))
    }

    /// The instance's state and public IPv4 address, from DescribeInstances.
    async fn describe(
        &self,
        region: &str,
        id: &str,
    ) -> Result<(String, Option<String>, Option<String>), CallError> {
        let xml = self
            .call(
                region,
                &[
                    ("Action", "DescribeInstances".into()),
                    ("InstanceId.1", id.into()),
                ],
            )
            .await?;
        let state = xml_tag(&xml, "instanceState")
            .and_then(|s| xml_tag(&s, "name"))
            .unwrap_or_default();
        Ok((
            state,
            xml_tag(&xml, "ipAddress"),
            xml_tag(&xml, "ipv6Address"),
        ))
    }

    pub fn new(
        regions: Vec<String>,
        spot_template: String,
        on_demand_template: Option<String>,
        zone: String,
    ) -> Result<Self> {
        anyhow::ensure!(!regions.is_empty(), "no EC2 regions configured");
        Ok(AwsEc2 {
            regions,
            spot_template,
            on_demand_template,
            zone,
            endpoints: BTreeMap::new(),
            health_scheme: "https".into(),
            health_port: 443,
            ready_timeout: Duration::from_secs(600),
            poll: Duration::from_secs(3),
            // A local HTTP request to the instance, not an EC2 API call: a ready renderer
            // is seen within 250 ms instead of up to `poll` (3 s) later.
            health_poll: HEALTH_POLL,
            pool_max: 0,
            pool_template: None,
            claimed: std::sync::Mutex::new(HashSet::new()),
            pool_lock: tokio::sync::Mutex::new(()),
            fleet: "stage".into(),
            networks: BTreeMap::new(),
            chain: CredentialChain::new()?,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .build()?,
        })
    }

    fn base(&self, region: &str) -> String {
        self.endpoints
            .get(region)
            .cloned()
            .unwrap_or_else(|| format!("https://ec2.{region}.amazonaws.com"))
    }

    async fn call(&self, region: &str, params: &[(&str, String)]) -> Result<String, CallError> {
        let mut body = String::from("Version=2016-11-15");
        for (k, v) in params {
            body.push('&');
            body.push_str(&super::sigv4::uri_encode(k));
            body.push('=');
            body.push_str(&super::sigv4::uri_encode(v));
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
        .map_err(CallError::Other)?;
        let xml = String::from_utf8_lossy(&bytes).to_string();
        if status.is_success() {
            return Ok(xml);
        }
        let code = xml_tag(&xml, "Code").unwrap_or_default();
        let msg = xml_tag(&xml, "Message").unwrap_or_default();
        if CAPACITY_CODES.contains(&code.as_str()) {
            Err(CallError::Capacity(format!("{region}: {code}: {msg}")))
        } else {
            Err(CallError::Other(anyhow::anyhow!(
                "EC2 in {region} answered {status}: {code}: {msg}"
            )))
        }
    }

    /// Whether stopped instances are kept for the next wake.
    fn pooling(&self) -> bool {
        self.pool_max > 0 && self.pool_template.is_some()
    }

    /// The region the pool lives in: the first fleet region.
    fn pool_region(&self) -> &str {
        &self.regions[0]
    }

    async fn call_ok(&self, region: &str, params: &[(&str, String)]) -> Result<String> {
        match self.call(region, params).await {
            Ok(x) => Ok(x),
            Err(CallError::Capacity(e)) => bail!("{e}"),
            Err(CallError::Other(e)) => Err(e),
        }
    }

    /// This fleet's tile instances in `region` in any of `states`.
    async fn fleet_instances(&self, region: &str, states: &[&str]) -> Result<Vec<Instance>> {
        let mut out = Vec::new();
        let mut next: Option<String> = None;
        loop {
            let mut params = vec![
                ("Action", "DescribeInstances".to_string()),
                ("Filter.1.Name", "tag:Purpose".into()),
                ("Filter.1.Value.1", "fcvm-fleet-tile".into()),
                ("Filter.2.Name", "instance-state-name".into()),
                ("Filter.3.Name", "tag:Fleet".into()),
                ("Filter.3.Value.1", self.fleet.clone()),
            ];
            let keys: Vec<String> = (1..=states.len())
                .map(|i| format!("Filter.2.Value.{i}"))
                .collect();
            for (k, st) in keys.iter().zip(states) {
                params.push((k.as_str(), st.to_string()));
            }
            if let Some(t) = &next {
                params.push(("NextToken", t.clone()));
            }
            let xml = self.call_ok(region, &params).await?;
            out.extend(parse_instances(&xml));
            next = xml_tag(&xml, "nextToken").filter(|t| !t.is_empty());
            if next.is_none() {
                break;
            }
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out.dedup_by(|a, b| a.id == b.id);
        Ok(out)
    }

    async fn one_instance(&self, region: &str, id: &str) -> Result<Instance> {
        let xml = self
            .call_ok(
                region,
                &[
                    ("Action", "DescribeInstances".into()),
                    ("InstanceId.1", id.into()),
                ],
            )
            .await?;
        parse_instances(&xml)
            .into_iter()
            .find(|i| i.id == id)
            .with_context(|| format!("DescribeInstances did not name {id}"))
    }

    /// Terminate `insts`, cancelling their persistent spot requests first: a persistent
    /// request whose instance is terminated launches a replacement. A one-time request
    /// closes when it is fulfilled and is left alone. When the requests cannot be read or
    /// cancelled nothing is terminated: the instances stay tagged for the next sweep.
    async fn terminate_all(&self, region: &str, insts: &[Instance]) -> Result<()> {
        if insts.is_empty() {
            return Ok(());
        }
        let requests: Vec<&String> = insts
            .iter()
            .filter_map(|i| i.spot_request.as_ref())
            .collect();
        if !requests.is_empty() {
            let mut params = vec![("Action", "DescribeSpotInstanceRequests".to_string())];
            let keys: Vec<String> = (1..=requests.len())
                .map(|i| format!("SpotInstanceRequestId.{i}"))
                .collect();
            for (k, r) in keys.iter().zip(&requests) {
                params.push((k.as_str(), (*r).clone()));
            }
            let xml = self
                .call_ok(region, &params)
                .await
                .context("reading the tiles' spot requests")?;
            let persistent = persistent_requests(&xml);
            if !persistent.is_empty() {
                let mut params = vec![("Action", "CancelSpotInstanceRequests".to_string())];
                let keys: Vec<String> = (1..=persistent.len())
                    .map(|i| format!("SpotInstanceRequestId.{i}"))
                    .collect();
                for (k, r) in keys.iter().zip(&persistent) {
                    params.push((k.as_str(), r.clone()));
                }
                self.call_ok(region, &params)
                    .await
                    .context("cancelling the tiles' persistent spot requests")?;
            }
        }
        let mut params = vec![("Action", "TerminateInstances".to_string())];
        let keys: Vec<String> = (1..=insts.len())
            .map(|i| format!("InstanceId.{i}"))
            .collect();
        for (k, i) in keys.iter().zip(insts) {
            params.push((k.as_str(), i.id.clone()));
        }
        self.call_ok(region, &params).await.map(|_| ())
    }

    /// The image the pool template launches now; a pooled instance of another image is stale.
    async fn pool_image(&self) -> Result<String> {
        let template = self.pool_template.as_deref().context("no pool template")?;
        let xml = self
            .call_ok(
                self.pool_region(),
                &[
                    ("Action", "DescribeLaunchTemplateVersions".into()),
                    ("LaunchTemplateName", template.into()),
                    ("LaunchTemplateVersion.1", "$Latest".into()),
                ],
            )
            .await?;
        xml_tag(&xml, "launchTemplateData")
            .and_then(|d| xml_tag(&d, "imageId"))
            .with_context(|| format!("launch template {template} names no image"))
    }

    async fn tag(&self, region: &str, id: &str, key: &str, value: &str) -> Result<()> {
        self.call_ok(
            region,
            &[
                ("Action", "CreateTags".into()),
                ("ResourceId.1", id.into()),
                ("Tag.1.Key", key.into()),
                ("Tag.1.Value", value.into()),
            ],
        )
        .await
        .map(|_| ())
    }

    async fn untag(&self, region: &str, id: &str, key: &str) -> Result<()> {
        self.call_ok(
            region,
            &[
                ("Action", "DeleteTags".into()),
                ("ResourceId.1", id.into()),
                ("Tag.1.Key", key.into()),
            ],
        )
        .await
        .map(|_| ())
    }

    /// Start one pooled instance for `tile`: the first of the fresh pooled ones that starts.
    async fn resume_from_pool(&self, tile: &str) -> Result<Option<Launched>> {
        let region = self.pool_region().to_string();
        let pooled: Vec<Instance> = self
            .fleet_instances(&region, &["stopped"])
            .await?
            .into_iter()
            .filter(|i| i.pooled)
            .collect();
        if pooled.is_empty() {
            return Ok(None);
        }
        let image = self.pool_image().await?;
        let oldest = chrono::Utc::now().timestamp() - POOL_MAX_AGE.as_secs() as i64;
        let (fresh, stale): (Vec<Instance>, Vec<Instance>) = pooled.into_iter().partition(|i| {
            i.image.as_deref() == Some(image.as_str()) && i.launched_at.is_some_and(|t| t >= oldest)
        });
        if !stale.is_empty() {
            let stale: Vec<Instance> = {
                let mut c = self.claimed.lock().unwrap();
                stale
                    .into_iter()
                    .filter(|i| c.insert(i.id.clone()))
                    .collect()
            };
            let ids: Vec<&str> = stale.iter().map(|i| i.id.as_str()).collect();
            tracing::info!(%region, instances = ?ids, %image, "terminating pooled tiles of an older image or past the pool's age");
            let done = self.terminate_all(&region, &stale).await;
            // Released either way: a terminated instance is not listed as stopped again, and
            // one whose termination failed is tried again at the next resume.
            {
                let mut c = self.claimed.lock().unwrap();
                for i in &stale {
                    c.remove(&i.id);
                }
            }
            done?;
        }
        for cand in fresh {
            if !self.claimed.lock().unwrap().insert(cand.id.clone()) {
                continue;
            }
            // The list can be older than the claim: the instance may have been started for
            // another tile and released since (a release holds its claim until its stop is
            // sent, and tags the instance FleetPool while it still runs). Start it only if
            // it is stopped and pooled now.
            match self.one_instance(&region, &cand.id).await {
                Ok(now) if now.state == "stopped" && now.pooled => {}
                Ok(_) | Err(_) => {
                    self.claimed.lock().unwrap().remove(&cand.id);
                    continue;
                }
            }
            match self
                .call(
                    &region,
                    &[
                        ("Action", "StartInstances".into()),
                        ("InstanceId.1", cand.id.clone()),
                    ],
                )
                .await
            {
                Ok(_) => {
                    // The instance is this tile's now, whatever the tags say; a failed tag
                    // only leaves it counted in the pool until it is released again.
                    if let Err(e) = self.tag(&region, &cand.id, "FleetTile", tile).await {
                        tracing::warn!(instance = %cand.id, error = %format!("{e:#}"), "tagging a resumed tile failed");
                    }
                    if let Err(e) = self.untag(&region, &cand.id, "FleetPool").await {
                        tracing::warn!(instance = %cand.id, error = %format!("{e:#}"), "untagging a resumed tile failed");
                    }
                    tracing::info!(%tile, %region, instance = %cand.id, "tile resumed from the pool");
                    return Ok(Some(Launched {
                        instance: format!("{region}/{}", cand.id),
                        frame_url: Some(format!("wss://{tile}.{}/frames", self.zone)),
                        ipv6: None,
                        ipv4: None,
                    }));
                }
                Err(e) => {
                    let why = match e {
                        CallError::Capacity(w) => w,
                        CallError::Other(e) => format!("{e:#}"),
                    };
                    // Left in the pool for a later wake (a spot request that is not yet
                    // disabled after its stop, or no capacity right now).
                    tracing::info!(%tile, instance = %cand.id, reason = %why, "a pooled tile did not start");
                    self.claimed.lock().unwrap().remove(&cand.id);
                }
            }
        }
        Ok(None)
    }

    /// Put `id` in the pool (tag, then stop) when there is room and its image is current;
    /// otherwise, or when it cannot be stopped, terminate it.
    async fn release_into_pool(&self, region: &str, id: &str) -> Result<()> {
        let inst = match self.one_instance(region, id).await {
            Ok(i) => i,
            Err(e) => {
                self.claimed.lock().unwrap().remove(id);
                return Err(e);
            }
        };
        let keep = {
            let _g = self.pool_lock.lock().await;
            let image = self.pool_image().await.ok();
            let room = self
                .fleet_instances(region, &["pending", "running", "stopping", "stopped"])
                .await
                .map(|v| v.iter().filter(|i| i.pooled && i.id != id).count() < self.pool_max)
                .unwrap_or(false);
            let oldest = chrono::Utc::now().timestamp() - POOL_MAX_AGE.as_secs() as i64;
            let keep = region == self.pool_region()
                && room
                && image.is_some()
                && inst.image == image
                && inst.launched_at.is_some_and(|t| t >= oldest);
            // An instance that could not be tagged into the pool is not pooled: it is
            // terminated below, not left running with no owner.
            keep && match self.tag(region, id, "FleetPool", &self.fleet).await {
                Ok(()) => true,
                Err(e) => {
                    tracing::warn!(%region, instance = %id, error = %format!("{e:#}"), "tagging a tile into the pool failed; terminating it");
                    false
                }
            }
        };
        let done = self.stop_or_terminate(region, id, inst, keep).await;
        // Held until the stop or terminate is sent, so a resume cannot claim it before.
        self.claimed.lock().unwrap().remove(id);
        done
    }

    async fn stop_or_terminate(
        &self,
        region: &str,
        id: &str,
        inst: Instance,
        keep: bool,
    ) -> Result<()> {
        if keep {
            match self
                .call_ok(
                    region,
                    &[
                        ("Action", "StopInstances".into()),
                        ("InstanceId.1", id.into()),
                    ],
                )
                .await
            {
                Ok(_) => {
                    tracing::info!(%region, instance = %id, "tile stopped into the pool");
                    return Ok(());
                }
                // One-time spot cannot be stopped: terminate it.
                Err(e) => {
                    tracing::info!(instance = %id, error = %format!("{e:#}"), "a tile could not be stopped; terminating it")
                }
            }
        }
        self.terminate_all(region, &[inst]).await
    }

    /// One RunInstances from `template`, in `subnet` when given. Naming a subnet means naming
    /// the whole interface the template describes (its group, a public IPv4 and one IPv6
    /// address), since a request's interface replaces the template's.
    async fn run(
        &self,
        region: &str,
        template: &str,
        tile: &str,
        subnet: Option<(&str, &str)>,
    ) -> Result<String, CallError> {
        let mut params: Vec<(&str, String)> = vec![
            ("Action", "RunInstances".into()),
            ("LaunchTemplate.LaunchTemplateName", template.into()),
            ("LaunchTemplate.Version", "$Latest".into()),
            ("MinCount", "1".into()),
            ("MaxCount", "1".into()),
            ("ClientToken", uuid::Uuid::new_v4().to_string()),
            ("TagSpecification.1.ResourceType", "instance".into()),
            ("TagSpecification.1.Tag.1.Key", "Purpose".into()),
            ("TagSpecification.1.Tag.1.Value", "fcvm-fleet-tile".into()),
            ("TagSpecification.1.Tag.2.Key", "FleetTile".into()),
            ("TagSpecification.1.Tag.2.Value", tile.into()),
            ("TagSpecification.1.Tag.3.Key", "Fleet".into()),
            ("TagSpecification.1.Tag.3.Value", self.fleet.clone()),
        ];
        if let Some((subnet, group)) = subnet {
            params.extend([
                ("NetworkInterface.1.DeviceIndex", "0".to_string()),
                ("NetworkInterface.1.SubnetId", subnet.to_string()),
                ("NetworkInterface.1.SecurityGroupId.1", group.to_string()),
                ("NetworkInterface.1.AssociatePublicIpAddress", "true".into()),
                ("NetworkInterface.1.Ipv6AddressCount", "1".into()),
                ("NetworkInterface.1.DeleteOnTermination", "true".into()),
            ]);
        }
        let xml = self.call(region, &params).await?;
        xml_tag(&xml, "instanceId")
            .ok_or_else(|| CallError::Other(anyhow::anyhow!("RunInstances named no instance")))
    }
}

/// `<region>/<instance id>`, how a launched tile's instance is named.
fn split(instance: &str) -> Result<(&str, &str)> {
    instance
        .split_once('/')
        .with_context(|| format!("{instance:?} is not <region>/<instance id>"))
}

#[async_trait]
impl Ec2Driver for AwsEc2 {
    async fn launch(&self, tile: &str) -> Result<Launched, LaunchError> {
        let mut refusals = Vec::new();
        // With the pool on, cold launches come from the pool template so the instance can
        // be stopped into the pool when the fleet sleeps.
        let first = match (&self.pool_template, self.pooling()) {
            (Some(t), true) => t,
            _ => &self.spot_template,
        };
        let templates = std::iter::once(first).chain(self.on_demand_template.iter());
        for template in templates {
            for region in &self.regions {
                // The region's subnets, one AZ each, or the template's own.
                let nets: Vec<Option<(&str, &str)>> = match self.networks.get(region) {
                    Some(n) => n
                        .subnets
                        .iter()
                        .map(|s| Some((s.as_str(), n.security_group.as_str())))
                        .collect(),
                    None => vec![None],
                };
                for net in nets {
                    let subnet = net.map(|(s, _)| s);
                    match self.run(region, template, tile, net).await {
                        Ok(id) => {
                            tracing::info!(%tile, %region, %template, ?subnet, instance = %id, "tile launched");
                            return Ok(Launched {
                                instance: format!("{region}/{id}"),
                                frame_url: Some(format!("wss://{tile}.{}/frames", self.zone)),
                                ipv6: None,
                                ipv4: None,
                            });
                        }
                        Err(CallError::Capacity(why)) => refusals.push(match subnet {
                            Some(s) => format!("{template} in {s} of {why}"),
                            None => format!("{template} in {why}"),
                        }),
                        Err(CallError::Other(e)) => return Err(LaunchError::Failed(e)),
                    }
                }
            }
        }
        Err(LaunchError::NoCapacity(refusals.join("; ")))
    }

    async fn resume(&self, tile: &str) -> Option<Launched> {
        if !self.pooling() {
            return None;
        }
        match self.resume_from_pool(tile).await {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(%tile, error = %format!("{e:#}"), "reading the tile pool failed; launching");
                None
            }
        }
    }

    async fn release(&self, instance: &str) -> Result<()> {
        let (region, id) = split(instance)?;
        if !self.pooling() {
            return self.terminate(instance).await;
        }
        self.release_into_pool(region, id).await
    }

    async fn ready(&self, tile: &str, launched: &Launched) -> Result<Launched> {
        let (region, id) = split(&launched.instance)?;
        let deadline = Instant::now() + self.ready_timeout;
        let mut up = launched.clone();
        // For the log: since this call, when the instance was first seen running with an
        // address, and when its renderer first answered /health.
        let t0 = Instant::now();
        let mut running_ms: Option<u128> = None;
        let mut health_ms: Option<u128> = None;
        // What the last describe said, and when the next one is due. Between describes
        // only `/health` is asked, every `health_poll`; a describe every `poll` still
        // notices an instance that is reclaimed while it boots.
        let mut running_at: Option<String> = None;
        let mut next_describe = Instant::now();
        loop {
            if Instant::now() >= deadline {
                bail!("{id} did not serve /health within {:?}", self.ready_timeout);
            }
            if Instant::now() >= next_describe {
                next_describe = Instant::now() + self.poll;
                match self.describe(region, id).await {
                    Ok((state, ipv4, ipv6)) => {
                        if matches!(
                            state.as_str(),
                            "shutting-down" | "terminated" | "stopping" | "stopped"
                        ) {
                            bail!("{id} is {state}");
                        }
                        up.ipv4 = ipv4;
                        up.ipv6 = ipv6;
                        running_at = match (&*state, &up.ipv4) {
                            ("running", Some(ip)) => Some(ip.clone()),
                            _ => None,
                        };
                        if running_at.is_some() {
                            running_ms.get_or_insert(t0.elapsed().as_millis());
                        }
                    }
                    Err(CallError::Capacity(e)) => bail!("{e}"),
                    Err(CallError::Other(e)) => {
                        tracing::debug!(error = %format!("{e:#}"), "describing a tile");
                    }
                }
            }
            if let Some(ip) = running_at.clone() {
                // A renderer that reports an error will not render: the launch fails
                // and the tile is retried.
                match self.probe(tile, &ip).await {
                    Health::Ready => {
                        health_ms.get_or_insert(t0.elapsed().as_millis());
                        tracing::info!(
                            %tile, instance = %id,
                            running_ms = running_ms.unwrap_or_default() as u64,
                            health_answered_ms = health_ms.unwrap_or_default() as u64,
                            ready_ms = t0.elapsed().as_millis() as u64,
                            "tile ready (ms since the launch was accepted)"
                        );
                        return Ok(up);
                    }
                    Health::Failed(e) => bail!("{id}'s renderer failed: {e}"),
                    Health::NotReady => {
                        health_ms.get_or_insert(t0.elapsed().as_millis());
                    }
                    Health::Silent(_) => {}
                }
                tokio::time::sleep(self.health_poll.min(self.poll)).await;
            } else {
                tokio::time::sleep(next_describe.saturating_duration_since(Instant::now())).await;
            }
        }
    }

    async fn post_scene(
        &self,
        tile: &str,
        launched: &Launched,
        body: &RendererScene,
    ) -> Result<()> {
        let (client, url) = self.renderer(tile, launched, "/scene")?;
        let r = client.post(&url).json(body).send().await?;
        if !r.status().is_success() {
            bail!("POST {url} answered {}", r.status());
        }
        Ok(())
    }

    async fn post_budget(&self, tile: &str, launched: &Launched, body: &BudgetBody) -> Result<()> {
        let (client, url) = self.renderer(tile, launched, "/budget")?;
        let r = client.post(&url).json(body).send().await?;
        if !r.status().is_success() {
            bail!("POST {url} answered {}", r.status());
        }
        Ok(())
    }

    async fn health(&self, tile: &str, launched: &Launched) -> Result<serde_json::Value> {
        let (client, url) = self.renderer(tile, launched, "/health")?;
        let r = client.get(&url).send().await?;
        if !r.status().is_success() {
            bail!("GET {url} answered {}", r.status());
        }
        Ok(r.json().await?)
    }

    async fn alive(&self, tile: &str, launched: &Launched) -> Result<Liveness> {
        let (region, id) = split(&launched.instance)?;
        let (state, ipv4, _) = match self.describe(region, id).await {
            Ok(d) => d,
            Err(CallError::Capacity(e)) => bail!("{e}"),
            Err(CallError::Other(e)) => return Err(e),
        };
        anyhow::ensure!(!state.is_empty(), "DescribeInstances named no state");
        if matches!(
            state.as_str(),
            "shutting-down" | "terminated" | "stopping" | "stopped"
        ) {
            return Ok(Liveness::Dead(format!("instance {id} is {state}")));
        }
        if state != "running" {
            return Ok(Liveness::Alive);
        }
        let Some(ip) = ipv4.or_else(|| launched.ipv4.clone()) else {
            return Ok(Liveness::Silent(format!("{id} has no address")));
        };
        Ok(match self.probe(tile, &ip).await {
            // A renderer that answers and is not ready is starting again: it is there.
            Health::Ready | Health::NotReady => Liveness::Alive,
            Health::Failed(e) => Liveness::Dead(format!("{id}'s renderer failed: {e}")),
            Health::Silent(e) => Liveness::Silent(e),
        })
    }

    /// Every instance of this fleet (tagged `Purpose=fcvm-fleet-tile` and `Fleet=<name>`)
    /// that is not terminated or shutting down, in every fleet region, terminated by id
    /// (spot requests cancelled first), except the pool: up to `pool_max` pooled instances
    /// that are stopped or stopping in the pool region stay for the next wake.
    async fn sweep(&self) -> Result<usize> {
        let mut total = 0;
        for region in &self.regions {
            let all = self
                .fleet_instances(region, &["pending", "running", "stopping", "stopped"])
                .await?;
            let mut kept = 0;
            let gone: Vec<Instance> = all
                .into_iter()
                .filter(|i| {
                    let keep = self.pooling()
                        && region == self.pool_region()
                        && i.pooled
                        && matches!(i.state.as_str(), "stopped" | "stopping")
                        && kept < self.pool_max;
                    kept += keep as usize;
                    !keep
                })
                .collect();
            if gone.is_empty() {
                continue;
            }
            self.terminate_all(region, &gone).await?;
            let ids: Vec<&str> = gone.iter().map(|i| i.id.as_str()).collect();
            tracing::info!(%region, instances = ?ids, pooled_kept = kept, "terminated tile instances");
            total += gone.len();
        }
        Ok(total)
    }

    async fn terminate(&self, instance: &str) -> Result<()> {
        let (region, id) = split(instance)?;
        let inst = self.one_instance(region, id).await?;
        let done = self.terminate_all(region, &[inst]).await;
        self.claimed.lock().unwrap().remove(id);
        done
    }
}
