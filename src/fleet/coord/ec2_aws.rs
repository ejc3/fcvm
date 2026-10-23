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
    pub poll: Duration,
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
                ],
            )
            .await?;
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
        loop {
            if Instant::now() >= deadline {
                bail!("{id} did not serve /health within {:?}", self.ready_timeout);
            }
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
                Err(CallError::Other(e)) => {
                    tracing::debug!(error = %format!("{e:#}"), "describing a tile");
                    tokio::time::sleep(self.poll).await;
                    continue;
                }
            };
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
            if state == "running" {
                if let Some(ip) = up.ipv4.clone() {
                    let addr: std::net::IpAddr = ip.parse().context("the instance's address")?;
                    let client = reqwest::Client::builder()
                        .resolve(&name, std::net::SocketAddr::new(addr, self.health_port))
                        .timeout(Duration::from_secs(5))
                        .build()?;
                    let url = format!(
                        "{}://{name}:{}/health",
                        self.health_scheme, self.health_port
                    );
                    if let Ok(r) = client.get(&url).send().await {
                        if r.status().is_success() {
                            return Ok(up);
                        }
                    }
                }
            }
            tokio::time::sleep(self.poll).await;
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
