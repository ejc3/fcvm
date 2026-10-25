//! The EC2 half on AWS: one instance per tile from the tile launch template (one-time spot),
//! tagged at launch (`Purpose=fcvm-fleet-tile`, `FleetTile=<tile>`), terminated by id. A
//! launch tries each fleet region in order; when every region refuses for capacity it tries
//! the on-demand template, if one is configured, and otherwise reports no capacity, which the
//! tile records as a capacity wait.
//!
//! The tile serves its renderer over TLS on 443 with the fleet's wildcard certificate, as
//! `<tile>.<zone>` (the coordinator's DNS writes that name). `ready` waits for the instance to
//! run, then for `/health` there, resolving the name to the instance's address itself so
//! readiness does not wait for DNS.

use super::aws::{signed, xml_tag, CredentialChain};
use super::ec2::{Ec2Driver, LaunchError, Launched};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use fleet_core::protocol::{BudgetBody, RendererScene};
use std::collections::BTreeMap;
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
    chain: CredentialChain,
    http: reqwest::Client,
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
        let addr: std::net::IpAddr = ip.parse().context("the tile's address")?;
        let name = format!("{tile}.{}", self.zone);
        let client = reqwest::Client::builder()
            .resolve(&name, std::net::SocketAddr::new(addr, self.health_port))
            .timeout(Duration::from_secs(5))
            .build()?;
        let url = format!("{}://{name}:{}{path}", self.health_scheme, self.health_port);
        Ok((client, url))
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
            fleet: "stage".into(),
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

    async fn run(&self, region: &str, template: &str, tile: &str) -> Result<String, CallError> {
        let xml = self
            .call(
                region,
                &[
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
                ],
            )
            .await?;
        xml_tag(&xml, "instanceId")
            .ok_or_else(|| CallError::Other(anyhow::anyhow!("RunInstances named no instance")))
    }
}

/// The text of every `<tag>` element in `xml`, in order.
fn all_tags(xml: &str, tag: &str) -> Vec<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find(&open) {
        rest = &rest[i + open.len()..];
        let Some(j) = rest.find(&close) else { break };
        out.push(rest[..j].to_string());
        rest = &rest[j + close.len()..];
    }
    out
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
        let templates = std::iter::once(&self.spot_template).chain(self.on_demand_template.iter());
        for template in templates {
            for region in &self.regions {
                match self.run(region, template, tile).await {
                    Ok(id) => {
                        tracing::info!(%tile, %region, %template, instance = %id, "tile launched");
                        return Ok(Launched {
                            instance: format!("{region}/{id}"),
                            frame_url: Some(format!("wss://{tile}.{}/frames", self.zone)),
                            ipv6: None,
                            ipv4: None,
                        });
                    }
                    Err(CallError::Capacity(why)) => refusals.push(format!("{template} in {why}")),
                    Err(CallError::Other(e)) => return Err(LaunchError::Failed(e)),
                }
            }
        }
        Err(LaunchError::NoCapacity(refusals.join("; ")))
    }

    async fn ready(&self, tile: &str, launched: &Launched) -> Result<Launched> {
        let (region, id) = split(&launched.instance)?;
        let deadline = Instant::now() + self.ready_timeout;
        let name = format!("{tile}.{}", self.zone);
        let mut up = launched.clone();
        // For the log: since this call, when the instance was first seen running with an
        // address, and when its renderer first answered /health.
        let t0 = Instant::now();
        let mut running_ms: Option<u128> = None;
        let mut health_ms: Option<u128> = None;
        // What the last describe said, and when the next one is due. Between describes
        // only `/health` is asked, every `health_poll`; a describe every `poll` still
        // notices an instance that is reclaimed while it boots.
        let mut running_at: Option<std::net::IpAddr> = None;
        let mut next_describe = Instant::now();
        loop {
            if Instant::now() >= deadline {
                bail!("{id} did not serve /health within {:?}", self.ready_timeout);
            }
            if Instant::now() >= next_describe {
                next_describe = Instant::now() + self.poll;
                match self
                    .call(
                        region,
                        &[
                            ("Action", "DescribeInstances".into()),
                            ("InstanceId.1", id.into()),
                        ],
                    )
                    .await
                {
                    Ok(xml) => {
                        let state = xml_tag(&xml, "instanceState")
                            .and_then(|s| xml_tag(&s, "name"))
                            .unwrap_or_default();
                        if matches!(
                            state.as_str(),
                            "shutting-down" | "terminated" | "stopping" | "stopped"
                        ) {
                            bail!("{id} is {state}");
                        }
                        up.ipv4 = xml_tag(&xml, "ipAddress");
                        up.ipv6 = xml_tag(&xml, "ipv6Address");
                        running_at = match (&*state, &up.ipv4) {
                            ("running", Some(ip)) => {
                                Some(ip.parse().context("the instance's address")?)
                            }
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
            if let Some(addr) = running_at {
                let client = reqwest::Client::builder()
                    .resolve(&name, std::net::SocketAddr::new(addr, self.health_port))
                    .timeout(Duration::from_secs(5))
                    .build()?;
                let url = format!(
                    "{}://{name}:{}/health",
                    self.health_scheme, self.health_port
                );
                // The renderer answers /health with 200 while CUDA starts and after it
                // has failed: ready means `ready` is true, and an `error` is a renderer
                // that will not render, so the launch fails and the tile is retried.
                if let Ok(r) = client.get(&url).send().await {
                    if r.status().is_success() {
                        health_ms.get_or_insert(t0.elapsed().as_millis());
                        if let Ok(h) = r.json::<serde_json::Value>().await {
                            if let Some(e) = h.get("error").filter(|e| !e.is_null()) {
                                bail!("{id}'s renderer failed: {e}");
                            }
                            if h["ready"] == serde_json::Value::Bool(true) {
                                tracing::info!(
                                    %tile, instance = %id,
                                    running_ms = running_ms.unwrap_or_default() as u64,
                                    health_answered_ms = health_ms.unwrap_or_default() as u64,
                                    ready_ms = t0.elapsed().as_millis() as u64,
                                    "tile ready (ms since the launch was accepted)"
                                );
                                return Ok(up);
                            }
                        }
                    }
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

    async fn alive(&self, launched: &Launched) -> Result<bool> {
        let (region, id) = split(&launched.instance)?;
        let xml = match self
            .call(
                region,
                &[
                    ("Action", "DescribeInstances".into()),
                    ("InstanceId.1", id.into()),
                ],
            )
            .await
        {
            Ok(x) => x,
            Err(CallError::Capacity(e)) => bail!("{e}"),
            Err(CallError::Other(e)) => return Err(e),
        };
        let state = xml_tag(&xml, "instanceState")
            .and_then(|s| xml_tag(&s, "name"))
            .context("DescribeInstances named no state")?;
        Ok(!matches!(
            state.as_str(),
            "shutting-down" | "terminated" | "stopping" | "stopped"
        ))
    }

    /// Every instance of this fleet (tagged `Purpose=fcvm-fleet-tile` and `Fleet=<name>`)
    /// that is not terminated or shutting down, in every fleet region, terminated by id.
    async fn sweep(&self) -> Result<usize> {
        let mut total = 0;
        for region in &self.regions {
            let mut ids = Vec::new();
            let mut next: Option<String> = None;
            loop {
                let mut params = vec![
                    ("Action", "DescribeInstances".to_string()),
                    ("Filter.1.Name", "tag:Purpose".into()),
                    ("Filter.1.Value.1", "fcvm-fleet-tile".into()),
                    ("Filter.2.Name", "instance-state-name".into()),
                    ("Filter.2.Value.1", "pending".into()),
                    ("Filter.2.Value.2", "running".into()),
                    ("Filter.2.Value.3", "stopping".into()),
                    ("Filter.2.Value.4", "stopped".into()),
                    ("Filter.3.Name", "tag:Fleet".into()),
                    ("Filter.3.Value.1", self.fleet.clone()),
                ];
                if let Some(t) = &next {
                    params.push(("NextToken", t.clone()));
                }
                let xml = match self.call(region, &params).await {
                    Ok(x) => x,
                    Err(CallError::Capacity(e)) => bail!("{e}"),
                    Err(CallError::Other(e)) => return Err(e),
                };
                ids.extend(all_tags(&xml, "instanceId"));
                next = xml_tag(&xml, "nextToken").filter(|t| !t.is_empty());
                if next.is_none() {
                    break;
                }
            }
            ids.sort();
            ids.dedup();
            if ids.is_empty() {
                continue;
            }
            let mut params = vec![("Action", "TerminateInstances".to_string())];
            let keys: Vec<String> = (1..=ids.len()).map(|i| format!("InstanceId.{i}")).collect();
            for (k, id) in keys.iter().zip(&ids) {
                params.push((k.as_str(), id.clone()));
            }
            match self.call(region, &params).await {
                Ok(_) => {}
                Err(CallError::Capacity(e)) => bail!("{e}"),
                Err(CallError::Other(e)) => return Err(e),
            }
            tracing::info!(%region, instances = ?ids, "terminated tile instances");
            total += ids.len();
        }
        Ok(total)
    }

    async fn terminate(&self, instance: &str) -> Result<()> {
        let (region, id) = split(instance)?;
        match self
            .call(
                region,
                &[
                    ("Action", "TerminateInstances".into()),
                    ("InstanceId.1", id.into()),
                ],
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(CallError::Capacity(e)) => bail!("{e}"),
            Err(CallError::Other(e)) => Err(e),
        }
    }
}
