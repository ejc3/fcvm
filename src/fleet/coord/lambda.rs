//! The Lambda tiles' driver (fleet/PROTOCOL.md, "Lambda tile"; `--lambda`). Each tile is one
//! function, `fcvm-lam-<fleet>-<n>`, on the stack's Lambda Managed Instances capacity provider,
//! running the lane image. It implements [`Ec2Driver`], so the EC2 reconciler runs the Lambda
//! tiles too ([`super::ec2::run`] with [`Ec2Driver::backing`] Lambda):
//!
//! - A launch writes the tile's generation and a tunnel credential minted for it into the
//!   function's configuration and publishes it (`PublishTo: LATEST_PUBLISHED`), so every
//!   relaunch is a new environment: publishing an unchanged function starts none. Lambda keeps
//!   the previous instance running, and a new version lands there when it is idle (17.6 to
//!   33.6 s to first frame, measured 2026-11-22), so a race is a restart on an instance the
//!   tile already had.
//! - Ready is the tile's tunnel for that generation up and its renderer's `/health` ready
//!   through it. The environment says which instance it runs on and when that instance booted
//!   ([`tunnel::Session`]): an instance the tile has not run on before, booted after the
//!   publish, is one Lambda launched for it, and the time from the publish to that boot is
//!   reported as the tile's `capacity_wait_ms` (kept out of the race time, as an EC2 tile's
//!   capacity wait is), with `new_instance` 1.
//! - Terminating a replaced version is Lambda's: publishing the next one retires it. A tile's
//!   function name is reused by every task the reconciler runs for the tile (a tile that
//!   leaves and comes back gets a new task while the old one may still be finishing), so the
//!   function belongs to the newest task ([`Ec2Driver::claim`]), and configuring, publishing
//!   and deleting it happen one at a time under its tile's lock. A task that ends deletes the
//!   function, whether its launch ran or never served, unless a newer task owns the tile; an
//!   older task's launch is refused. The sweep at start and at shutdown deletes every
//!   function of this fleet's names, and the one at shutdown also refuses every later launch.
//! - Scene, budget and health go over the tile's tunnel to the renderer's HTTP listener.

use super::aws::{signed, CredentialChain};
use super::ec2::{Backing, Ec2Driver, LaunchError, Launched, Liveness};
use crate::fleet::tunnel::{self, Endpoint, Port};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use fleet_core::protocol::{BudgetBody, RendererScene};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The most Lambda tiles a fleet has (the AWS half's cap), which the sweep covers.
pub const MAX_TILES: u32 = 40;
/// How long a tile's credential is valid: longer than any wake.
const CREDENTIAL_LIFE: Duration = Duration::from_secs(7 * 24 * 3600);
/// Publish to the new environment's tunnel and ready renderer, at most (3x the measured
/// 96 s publish on an empty provider).
const READY_TIMEOUT: Duration = Duration::from_secs(300);
/// How long a configuration update may stay in progress.
const UPDATE_TIMEOUT: Duration = Duration::from_secs(120);

/// The Lambda API (REST, JSON), signed with the task's role.
pub struct LambdaApi {
    chain: CredentialChain,
    http: reqwest::Client,
    region: String,
    base: String,
}

/// An API error's type (`ResourceNotFoundException`, ...) and message.
#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub kind: String,
    pub message: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Lambda answered {}: {} {}",
            self.status, self.kind, self.message
        )
    }
}
impl std::error::Error for ApiError {}

impl LambdaApi {
    pub fn new(region: &str) -> Result<Self> {
        LambdaApi::with_endpoint(region, &format!("https://lambda.{region}.amazonaws.com"))
    }

    pub fn with_endpoint(region: &str, base: &str) -> Result<Self> {
        Ok(LambdaApi {
            chain: CredentialChain::new()?,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()?,
            region: region.into(),
            base: base.into(),
        })
    }

    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        query: &str,
        body: Option<Value>,
    ) -> Result<Value> {
        let bytes = body
            .map(|b| serde_json::to_vec(&b))
            .transpose()?
            .unwrap_or_default();
        let (status, out) = signed(
            &self.chain,
            &self.http,
            &self.base,
            method,
            path,
            query,
            &self.region,
            "lambda",
            &[("content-type", "application/json".into())],
            bytes,
        )
        .await?;
        let v: Value = serde_json::from_slice(&out).unwrap_or(Value::Null);
        if !status.is_success() {
            // The error's type and message only: a request body is never echoed.
            let kind = v
                .get("Type")
                .or_else(|| v.get("__type"))
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();
            let message = v
                .get("message")
                .or_else(|| v.get("Message"))
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();
            return Err(ApiError {
                status: status.as_u16(),
                kind,
                message,
            }
            .into());
        }
        Ok(v)
    }

    fn path(name: &str, rest: &str) -> String {
        format!(
            "/2015-03-31/functions/{}{rest}",
            super::sigv4::uri_encode(name)
        )
    }

    /// The function's configuration, or None when it does not exist.
    pub async fn configuration(&self, name: &str) -> Result<Option<Value>> {
        match self
            .call(
                reqwest::Method::GET,
                &Self::path(name, "/configuration"),
                "",
                None,
            )
            .await
        {
            Ok(v) => Ok(Some(v)),
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub async fn create(&self, body: Value) -> Result<Value> {
        self.call(
            reqwest::Method::POST,
            "/2015-03-31/functions",
            "",
            Some(body),
        )
        .await
    }

    pub async fn update_environment(
        &self,
        name: &str,
        vars: &BTreeMap<String, String>,
    ) -> Result<()> {
        self.call(
            reqwest::Method::PUT,
            &Self::path(name, "/configuration"),
            "",
            Some(json!({"Environment": {"Variables": vars}})),
        )
        .await
        .map(|_| ())
    }

    pub async fn publish(&self, name: &str) -> Result<Value> {
        self.call(
            reqwest::Method::POST,
            &Self::path(name, "/versions"),
            "",
            Some(json!({"PublishTo": "LATEST_PUBLISHED"})),
        )
        .await
    }

    pub async fn delete(&self, name: &str) -> Result<bool> {
        match self
            .call(reqwest::Method::DELETE, &Self::path(name, ""), "", None)
            .await
        {
            Ok(_) => Ok(true),
            Err(e) if is_not_found(&e) => Ok(false),
            Err(e) => Err(e),
        }
    }
}

fn is_not_found(e: &anyhow::Error) -> bool {
    e.downcast_ref::<ApiError>()
        .is_some_and(|a| a.status == 404 || a.kind == "ResourceNotFoundException")
}

/// What the driver needs to create and run the tiles' functions.
#[derive(Debug, Clone)]
pub struct Plan {
    /// `--fleet-name`: the functions are `fcvm-lam-<fleet>-<n>`.
    pub fleet: String,
    pub image: String,
    pub role: String,
    pub capacity_provider: String,
    /// The tunnel endpoint's name (and port) the environments dial.
    pub tunnel: String,
    /// The zone the tiles' names are under (`lam-<n>.<zone>`, the frame URL's host).
    pub zone: String,
    pub tls_secret: String,
    pub tls_secret_region: String,
    /// The coordinator task's public addresses, where the tiles' launch names point.
    pub ipv6: Option<String>,
    pub ipv4: Option<String>,
}

impl Plan {
    pub fn function(&self, tile: &str) -> String {
        format!(
            "fcvm-lam-{}-{}",
            self.fleet,
            tile.trim_start_matches("lam-")
        )
    }

    /// The function's configuration for one launch of one generation. `launch` differs on
    /// every launch, so a launch of a generation that already ran (a tile whose renderer
    /// stopped answering) still publishes a new version: an unchanged one starts nothing.
    pub fn environment(
        &self,
        tile: &str,
        generation: u64,
        launch: u64,
        credential: &str,
    ) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("FLEET_TILE_SLOT".into(), tile.into()),
            ("FLEET_TILE_LAUNCH".into(), launch.to_string()),
            ("FLEET_TILE_GENERATION".into(), generation.to_string()),
            ("FLEET_TILE_CREDENTIAL".into(), credential.into()),
            ("FLEET_TILE_TUNNEL".into(), self.tunnel.clone()),
            ("FLEET_TLS_SECRET".into(), self.tls_secret.clone()),
            ("FLEET_SECRET_REGION".into(), self.tls_secret_region.clone()),
        ])
    }

    /// CreateFunction's body: the lane image on the capacity provider, with a T4's memory
    /// (spike 2026-11-22: 8 GiB, 16 GiB of accelerator memory, x86_64).
    pub fn create_body(&self, name: &str, vars: &BTreeMap<String, String>) -> Value {
        json!({
            "FunctionName": name,
            "PackageType": "Image",
            "Code": {"ImageUri": self.image},
            "Role": self.role,
            "Architectures": ["x86_64"],
            "MemorySize": 8192,
            "Timeout": 900,
            "Environment": {"Variables": vars},
            "CapacityProviderConfig": {"LambdaManagedInstancesCapacityProviderConfig": {
                "CapacityProviderArn": self.capacity_provider,
                "PerExecutionEnvironmentMaxConcurrency": 2,
                "ExecutionEnvironmentMemoryGiBPerVCpu": 4,
            }},
            "AcceleratorConfig": {"AcceleratorMemorySize": 16},
            // The daily budgets and the capacity provider's propagated tags filter on Purpose
            // and Fleet (ejc3/aws fcvm-fleet-lambda.tf).
            "Tags": {"Project": "fcvm-fleet", "Purpose": "fcvm-fleet-tile", "Fleet": self.fleet},
        })
    }
}

/// What the instance of a new environment says about the wait for it: whether the tile has
/// run on that instance before, and, for a new one booted after the publish, how long Lambda
/// took to bring it up. `known` is every instance the tile ran on this wake.
pub fn instance_timings(
    known: &BTreeSet<String>,
    instance: Option<&str>,
    instance_boot_ms: Option<u64>,
    published_ms: u64,
) -> BTreeMap<String, u64> {
    let mut t = BTreeMap::new();
    let new = instance.is_none_or(|i| !known.contains(i));
    t.insert("new_instance".into(), new as u64);
    if let (true, Some(boot)) = (new, instance_boot_ms) {
        if boot > published_ms {
            t.insert("capacity_wait_ms".into(), boot - published_ms);
        }
    }
    t
}

/// `--lambda-image`: the lane image by digest, `<repository>@sha256:<hex>`. Every function
/// this coordinator creates runs that one digest, which fleet/lambda/release.py keeps in the
/// shared repository while the stack runs it (fleet/PROTOCOL.md, "Which lane image a stack
/// runs"). A tag is refused: the protected tags move, so a function created after a selection
/// would run another image than one created before it.
pub fn lane_image(image: &str) -> Result<String> {
    let valid = image.split_once("@sha256:").is_some_and(|(name, hex)| {
        !name.is_empty()
            && !name.contains(|c: char| c.is_whitespace() || c == '@')
            && !name
                .rsplit('/')
                .next()
                .is_some_and(|last| last.contains(':'))
            && hex.len() == 64
            && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    });
    anyhow::ensure!(
        valid,
        "--lambda-image must name the lane image by digest: <repository>@sha256:<64 hex>"
    );
    Ok(image.to_string())
}

/// The parts of the Lambda API the driver calls, so tests can stand in for it.
#[async_trait]
pub trait Functions: Send + Sync {
    /// Make the function hold this configuration, creating it if it does not exist, and
    /// return once Lambda took it.
    async fn put(&self, name: &str, create: Value, vars: &BTreeMap<String, String>) -> Result<()>;
    async fn publish(&self, name: &str) -> Result<()>;
    /// Delete it; false when it did not exist.
    async fn delete(&self, name: &str) -> Result<bool>;
    /// The published version's state and reason (`Active`, `Pending`, `Failed`).
    async fn published_state(&self, name: &str) -> Result<(String, Option<String>)>;
}

#[async_trait]
impl Functions for LambdaApi {
    async fn put(&self, name: &str, create: Value, vars: &BTreeMap<String, String>) -> Result<()> {
        match self.configuration(name).await? {
            None => {
                self.create(create).await?;
            }
            Some(_) => self.update_environment(name, vars).await?,
        }
        let deadline = Instant::now() + UPDATE_TIMEOUT;
        loop {
            let c = self
                .configuration(name)
                .await?
                .context("the function went away")?;
            let state = c["State"].as_str().unwrap_or("");
            let update = c["LastUpdateStatus"].as_str().unwrap_or("");
            if state == "Failed" || update == "Failed" {
                bail!(
                    "the function's configuration failed: {} {}",
                    c["StateReasonCode"].as_str().unwrap_or(""),
                    c["LastUpdateStatusReasonCode"].as_str().unwrap_or("")
                );
            }
            if state != "Pending" && update != "InProgress" {
                return Ok(());
            }
            if Instant::now() > deadline {
                bail!("the function's configuration did not settle within {UPDATE_TIMEOUT:?}");
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    async fn publish(&self, name: &str) -> Result<()> {
        LambdaApi::publish(self, name).await.map(|_| ())
    }

    async fn delete(&self, name: &str) -> Result<bool> {
        LambdaApi::delete(self, name).await
    }

    async fn published_state(&self, name: &str) -> Result<(String, Option<String>)> {
        let v = self
            .call(
                reqwest::Method::GET,
                &Self::path(name, "/configuration"),
                "Qualifier=%24LATEST.PUBLISHED",
                None,
            )
            .await?;
        Ok((
            v["State"].as_str().unwrap_or("").to_string(),
            v["StateReasonCode"].as_str().map(str::to_string),
        ))
    }
}

/// Whether a reason Lambda gives for a failed or pending version names capacity.
pub fn is_capacity(reason: &str) -> bool {
    let r = reason.to_ascii_lowercase();
    r.contains("capacity") || r.contains("insufficient")
}

/// Per tile: the generation of its latest launch and the timings measured for it.
type Timings = HashMap<String, (u64, BTreeMap<String, u64>)>;
/// Per tile: the generation last published, when (Unix ms), and the environment that held
/// the tile then.
type Published = HashMap<String, (u64, u64, Option<String>)>;

pub struct LambdaDriver {
    plan: Plan,
    api: Arc<dyn Functions>,
    endpoint: Arc<Endpoint>,
    key: ring::hmac::Key,
    scene_token: super::scene_token::SceneToken,
    /// Per tile: the instances it ran on this wake, and the timings of its latest launch.
    seen: Mutex<HashMap<String, BTreeSet<String>>>,
    timings: Mutex<Timings>,
    /// Per tile: the generation last published, when, and the environment that held the tile
    /// then (a relaunch of the same generation is ready only on another one).
    published: Mutex<Published>,
    /// Starts at the coordinator's start time, so no launch repeats an earlier coordinator's.
    launches: std::sync::atomic::AtomicU64,
    ready_timeout: Duration,
    /// Per tile: the tile task that owns its function ([`Ec2Driver::claim`]), behind the lock
    /// every change to the function is made under (configure and publish, delete), so those
    /// happen one at a time per tile. A launch or a cleanup for any other task is refused: the
    /// function's name is reused across tasks, and an earlier task's late cleanup would
    /// otherwise delete a launch of the task that replaced it.
    owners: Mutex<HashMap<String, Arc<tokio::sync::Mutex<Option<u64>>>>>,
    /// The coordinator is stopping: no task owns a tile again.
    closed: std::sync::atomic::AtomicBool,
}

/// The owner used by direct calls ([`Ec2Driver::launch_generation`], [`Ec2Driver::release`]),
/// which no reconciler task makes.
const DIRECT: u64 = 0;

impl LambdaDriver {
    pub fn new(
        plan: Plan,
        api: Arc<dyn Functions>,
        endpoint: Arc<Endpoint>,
        key: ring::hmac::Key,
        scene_token: super::scene_token::SceneToken,
    ) -> Self {
        LambdaDriver {
            plan,
            api,
            endpoint,
            key,
            scene_token,
            seen: Mutex::default(),
            timings: Mutex::default(),
            published: Mutex::default(),
            launches: std::sync::atomic::AtomicU64::new(crate::fleet::now_ms()),
            ready_timeout: READY_TIMEOUT,
            owners: Mutex::default(),
            closed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// `tile`'s owner and the lock its function's changes are made under.
    fn owner(&self, tile: &str) -> Arc<tokio::sync::Mutex<Option<u64>>> {
        self.owners
            .lock()
            .unwrap()
            .entry(tile.to_string())
            .or_default()
            .clone()
    }

    /// Delete `tile`'s function and end its tunnel, with its owner lock held (`owner`), so no
    /// task owns the tile after.
    async fn delete_locked(&self, tile: &str, owner: &mut Option<u64>) -> Result<bool> {
        *owner = None;
        self.endpoint.forget(tile);
        self.seen.lock().unwrap().remove(tile);
        self.api.delete(&self.plan.function(tile)).await
    }

    /// Configure and publish `tile`'s function for `generation`, with its owner lock held.
    async fn launch_locked(&self, tile: &str, generation: u64) -> Result<Launched, LaunchError> {
        let name = self.plan.function(tile);
        let expires = crate::fleet::now_ms() + CREDENTIAL_LIFE.as_millis() as u64;
        let credential = tunnel::mint(&self.key, tile, generation, expires);
        let launch = self
            .launches
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let vars = self.plan.environment(tile, generation, launch, &credential);
        // From now on the endpoint takes this generation's tunnel only.
        self.endpoint.want(tile, generation);
        self.api
            .put(&name, self.plan.create_body(&name, &vars), &vars)
            .await
            .map_err(LaunchError::Failed)?;
        let before = self
            .endpoint
            .sessions()
            .borrow()
            .get(tile)
            .map(|s| s.env.clone());
        let published_ms = crate::fleet::now_ms();
        self.api.publish(&name).await.map_err(LaunchError::Failed)?;
        self.published
            .lock()
            .unwrap()
            .insert(tile.to_string(), (generation, published_ms, before));
        Ok(Launched {
            instance: self.instance_name(tile, generation),
            frame_url: Some(format!("wss://{tile}.{}/frames", self.plan.zone)),
            ipv6: self.plan.ipv6.clone(),
            ipv4: self.plan.ipv4.clone(),
        })
    }

    pub fn with_ready_timeout(mut self, d: Duration) -> Self {
        self.ready_timeout = d;
        self
    }

    /// `<region>/<function>#<generation>`: the launch the reconciler holds.
    fn instance_name(&self, tile: &str, generation: u64) -> String {
        format!("{}#{generation}", self.plan.function(tile))
    }

    fn generation_of(instance: &str) -> Result<u64> {
        instance
            .rsplit_once('#')
            .and_then(|(_, g)| g.parse().ok())
            .with_context(|| format!("{instance:?} is not <function>#<generation>"))
    }

    /// One HTTP request to the tile's renderer, over a tunnel stream.
    async fn request(
        &self,
        tile: &str,
        method: hyper::Method,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<(u16, Vec<u8>)> {
        use http_body_util::{BodyExt, Full};
        let stream = self.endpoint.open(tile, Port::Http).await?;
        let io = hyper_util::rt::TokioIo::new(stream);
        let (mut send, conn) = hyper::client::conn::http1::handshake(io).await?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        let mut req = hyper::Request::builder()
            .method(method)
            .uri(path)
            .header("host", format!("{tile}.{}", self.plan.zone));
        if let Some(t) = self.scene_token.get() {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        let req = match body {
            Some(b) => req
                .header("content-type", "application/json")
                .body(Full::new(hyper::body::Bytes::from(b)))?,
            None => req.body(Full::new(hyper::body::Bytes::new()))?,
        };
        let r = tokio::time::timeout(Duration::from_secs(10), async {
            let resp = send.send_request(req).await?;
            let status = resp.status().as_u16();
            let bytes = resp.into_body().collect().await?.to_bytes().to_vec();
            anyhow::Ok((status, bytes))
        })
        .await
        .context("the renderer did not answer in 10 s")??;
        Ok(r)
    }

    async fn health_of(&self, tile: &str) -> Result<Value> {
        let (status, body) = self
            .request(tile, hyper::Method::GET, "/health", None)
            .await?;
        if status != 200 {
            bail!("GET /health answered {status}");
        }
        Ok(serde_json::from_slice(&body)?)
    }
}

#[async_trait]
impl Ec2Driver for LambdaDriver {
    fn backing(&self) -> Backing {
        Backing::Lambda
    }

    async fn launch(&self, tile: &str) -> Result<Launched, LaunchError> {
        self.launch_generation(tile, 0).await
    }

    /// A launch no reconciler task makes (a test, a tool): it takes the tile for itself.
    async fn launch_generation(
        &self,
        tile: &str,
        generation: u64,
    ) -> Result<Launched, LaunchError> {
        self.claim(tile, DIRECT).await;
        self.launch_owned(tile, generation, DIRECT).await
    }

    async fn claim(&self, tile: &str, owner: u64) {
        let lock = self.owner(tile);
        let mut o = lock.lock().await;
        if !self.closed.load(std::sync::atomic::Ordering::SeqCst) {
            *o = Some(owner);
        }
    }

    async fn launch_owned(
        &self,
        tile: &str,
        generation: u64,
        owner: u64,
    ) -> Result<Launched, LaunchError> {
        let lock = self.owner(tile);
        let o = lock.lock().await;
        if *o != Some(owner) {
            return Err(LaunchError::Failed(anyhow::anyhow!(
                "{tile} belongs to another tile task, or the coordinator is stopping"
            )));
        }
        let r = self.launch_locked(tile, generation).await;
        drop(o);
        r
    }

    async fn ready(&self, tile: &str, launched: &Launched) -> Result<Launched> {
        let generation = Self::generation_of(&launched.instance)?;
        let name = self.plan.function(tile);
        let deadline = Instant::now() + self.ready_timeout;
        let (published_ms, before) = self
            .published
            .lock()
            .unwrap()
            .get(tile)
            .filter(|(g, _, _)| *g == generation)
            .map(|(_, t, b)| (Some(*t), b.clone()))
            .unwrap_or_default();
        let mut sessions = self.endpoint.sessions();
        let mut next_state = Instant::now();
        let session = loop {
            if let Some(s) = sessions
                .borrow_and_update()
                .get(tile)
                .filter(|s| s.generation == generation && Some(&s.env) != before.as_ref())
                .cloned()
            {
                break s;
            }
            if Instant::now() >= deadline {
                bail!("{name}'s environment for generation {generation} did not dial its tunnel within {:?}", self.ready_timeout);
            }
            if Instant::now() >= next_state {
                next_state = Instant::now() + Duration::from_secs(5);
                match self.api.published_state(&name).await {
                    Ok((state, reason)) if state == "Failed" => {
                        bail!(
                            "{name}'s published version failed: {}",
                            reason.unwrap_or_default()
                        )
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::debug!(tile, error = %format!("{e:#}"), "reading the published version")
                    }
                }
            }
            let _ = tokio::time::timeout(Duration::from_secs(1), sessions.changed()).await;
        };
        loop {
            match self.health_of(tile).await {
                Ok(h) if h.get("error").is_some_and(|e| !e.is_null()) => {
                    bail!("{name}'s renderer failed: {}", h["error"])
                }
                Ok(h) if h["ready"] == Value::Bool(true) => break,
                _ => {}
            }
            if Instant::now() >= deadline {
                bail!(
                    "{name}'s renderer was not ready within {:?}",
                    self.ready_timeout
                );
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let published_ms = published_ms.unwrap_or(session.since_ms);
        let t = {
            let mut seen = self.seen.lock().unwrap();
            let known = seen.entry(tile.to_string()).or_default();
            let t = instance_timings(
                known,
                session.instance.as_deref(),
                session.instance_boot_ms,
                published_ms,
            );
            if let Some(i) = &session.instance {
                known.insert(i.clone());
            }
            t
        };
        tracing::info!(%tile, generation, env = %session.env, new_instance = t["new_instance"], capacity_wait_ms = ?t.get("capacity_wait_ms"), "Lambda tile ready");
        self.timings
            .lock()
            .unwrap()
            .insert(tile.to_string(), (generation, t));
        Ok(launched.clone())
    }

    fn launch_timings(&self, tile: &str, launched: &Launched) -> BTreeMap<String, u64> {
        let Ok(generation) = Self::generation_of(&launched.instance) else {
            return BTreeMap::new();
        };
        self.timings
            .lock()
            .unwrap()
            .get(tile)
            .filter(|(g, _)| *g == generation)
            .map(|(_, t)| t.clone())
            .unwrap_or_default()
    }

    async fn terminate(&self, instance: &str) -> Result<()> {
        // A published version is replaced by the next publish; Lambda retires its environment.
        tracing::debug!(%instance, "a Lambda tile's version is retired by the next publish");
        Ok(())
    }

    /// A release no reconciler task makes: deletes the tile's function whoever owns it.
    async fn release(&self, instance: &str) -> Result<()> {
        let name = instance
            .rsplit_once('#')
            .map(|(n, _)| n)
            .unwrap_or(instance);
        let tile = name
            .rsplit_once('-')
            .map(|(_, n)| format!("lam-{n}"))
            .with_context(|| format!("{instance:?} names no Lambda tile"))?;
        let lock = self.owner(&tile);
        let mut o = lock.lock().await;
        self.delete_locked(&tile, &mut o).await.map(|_| ())
    }

    /// The tile task `owner` ended: delete the tile's function, whether a launch of it runs or
    /// never served, unless another task owns the tile now (it rejoined the assignment, and
    /// the function holds that task's launch).
    async fn retire(&self, tile: &str, owner: u64, _current: Option<&str>) -> Result<()> {
        let lock = self.owner(tile);
        let mut o = lock.lock().await;
        if *o != Some(owner) {
            tracing::debug!(
                tile,
                "a retired tile task leaves the tile's function to its owner"
            );
            return Ok(());
        }
        self.delete_locked(tile, &mut o).await.map(|_| ())
    }

    async fn post_scene(&self, tile: &str, _l: &Launched, body: &RendererScene) -> Result<()> {
        let (status, _) = self
            .request(
                tile,
                hyper::Method::POST,
                "/scene",
                Some(serde_json::to_vec(body)?),
            )
            .await?;
        if !(200..300).contains(&status) {
            bail!("POST /scene answered {status}");
        }
        Ok(())
    }

    async fn post_budget(&self, tile: &str, _l: &Launched, body: &BudgetBody) -> Result<()> {
        let (status, _) = self
            .request(
                tile,
                hyper::Method::POST,
                "/budget",
                Some(serde_json::to_vec(body)?),
            )
            .await?;
        if !(200..300).contains(&status) {
            bail!("POST /budget answered {status}");
        }
        Ok(())
    }

    async fn health(&self, tile: &str, _l: &Launched) -> Result<Value> {
        self.health_of(tile).await
    }

    async fn alive(&self, tile: &str, launched: &Launched) -> Result<Liveness> {
        let generation = Self::generation_of(&launched.instance)?;
        let held = self
            .endpoint
            .sessions()
            .borrow()
            .get(tile)
            .is_some_and(|s| s.generation == generation);
        if !held {
            return Ok(Liveness::Silent(format!(
                "{tile} has no tunnel for generation {generation}"
            )));
        }
        match self.health_of(tile).await {
            Ok(h) if h.get("error").is_some_and(|e| !e.is_null()) => Ok(Liveness::Dead(format!(
                "the renderer reports {}",
                h["error"]
            ))),
            Ok(_) => Ok(Liveness::Alive),
            Err(e) => Ok(Liveness::Silent(format!("{e:#}"))),
        }
    }

    /// Delete every function of this fleet's names, each under its tile's lock, so a launch
    /// in progress finishes first and none is owned after.
    async fn sweep(&self) -> Result<usize> {
        let mut n = 0;
        for k in 1..=MAX_TILES {
            let tile = format!("lam-{k}");
            let lock = self.owner(&tile);
            let mut o = lock.lock().await;
            if self.delete_locked(&tile, &mut o).await? {
                n += 1;
            }
        }
        Ok(n)
    }

    async fn sweep_at_shutdown(&self) -> Result<usize> {
        self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
        self.sweep().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::tunnel::{Dialer, Identity, Local, Stream};
    use tokio::sync::watch;

    #[test]
    fn a_new_instance_booted_after_the_publish_is_a_capacity_wait() {
        let mut known = BTreeSet::new();
        let t = instance_timings(&known, Some("i-a"), Some(10_500), 10_000);
        assert_eq!(
            t,
            BTreeMap::from([("new_instance".into(), 1), ("capacity_wait_ms".into(), 500)])
        );
        known.insert("i-a".to_string());
        // Back on an instance the tile ran on: a restart, no wait kept out.
        let t = instance_timings(&known, Some("i-a"), Some(10_500), 20_000);
        assert_eq!(t, BTreeMap::from([("new_instance".into(), 0)]));
        // A new instance that was up before the publish (another tile's warm one): new,
        // but Lambda launched nothing for this tile.
        let t = instance_timings(&known, Some("i-b"), Some(5_000), 20_000);
        assert_eq!(t, BTreeMap::from([("new_instance".into(), 1)]));
    }

    #[test]
    fn every_launch_publishes_a_configuration_of_its_own() {
        let plan = plan();
        let a = plan.environment("lam-2", 7, 100, "c");
        let b = plan.environment("lam-2", 7, 101, "c");
        assert_ne!(
            a, b,
            "a relaunch of the same generation must change the configuration"
        );
        assert_eq!(a["FLEET_TILE_SLOT"], "lam-2");
        assert_eq!(a["FLEET_TILE_GENERATION"], "7");
        assert_eq!(a["FLEET_TILE_TUNNEL"], "tunnel.x.example");
        assert_eq!(plan.function("lam-2"), "fcvm-lam-test-2");
        let body = plan.create_body("fcvm-lam-test-2", &a);
        assert_eq!(body["PackageType"], "Image");
        assert_eq!(body["AcceleratorConfig"]["AcceleratorMemorySize"], 16);
    }

    #[test]
    fn a_function_carries_the_tags_its_budget_filters_on() {
        let body = plan().create_body("fcvm-lam-test-2", &BTreeMap::new());
        assert_eq!(
            body["Tags"],
            json!({"Project": "fcvm-fleet", "Purpose": "fcvm-fleet-tile", "Fleet": "test"})
        );
    }

    #[test]
    fn the_lane_image_is_named_by_digest() {
        let digest = format!("sha256:{}", "0a".repeat(32));
        let image = format!("example.invalid/fcvm-fleet/lambda-tile@{digest}");
        assert_eq!(lane_image(&image).unwrap(), image);
        for refused in [
            "example.invalid/fcvm-fleet/lambda-tile:keep-stage-current".to_string(),
            "example.invalid/fcvm-fleet/lambda-tile:rev-1".to_string(),
            format!("@{digest}"),
            format!("example.invalid/lane@sha256:{}", "0A".repeat(32)),
            format!("example.invalid/lane@sha256:{}", "0a".repeat(31)),
            format!("example.invalid/lane:1@{digest}"),
            format!("example.invalid/la ne@{digest}"),
        ] {
            assert!(lane_image(&refused).is_err(), "{refused} was taken");
        }
    }

    fn plan() -> Plan {
        Plan {
            fleet: "test".into(),
            image: "example.invalid/lane:1".into(),
            role: "example-role".into(),
            capacity_provider: "example-provider".into(),
            tunnel: "tunnel.x.example".into(),
            zone: "x.example".into(),
            tls_secret: "example-secret".into(),
            tls_secret_region: "us-east-2".into(),
            ipv6: Some("2001:db8::1".into()),
            ipv4: Some("192.0.2.1".into()),
        }
    }

    struct InProcess(Arc<Endpoint>);

    #[async_trait]
    impl Dialer for InProcess {
        async fn dial(&self) -> Result<Stream> {
            let (a, b) = tokio::io::duplex(1 << 16);
            let ep = self.0.clone();
            tokio::spawn(async move {
                let _ = ep.serve(b).await;
            });
            Ok(Box::new(a))
        }
    }

    /// Requests the stand-in served: method and path, and the authorization header.
    type Seen = Arc<Mutex<Vec<(String, Option<String>)>>>;

    /// The renderer stand-in's HTTP listener: `/health` ready, and every request recorded.
    struct Renderer {
        addr: std::net::SocketAddr,
        seen: Seen,
    }

    async fn renderer() -> Renderer {
        use axum::extract::State;
        let seen: Seen = Arc::default();
        let app = axum::Router::new()
            .fallback(
                |State(seen): State<Seen>, req: axum::extract::Request| async move {
                    let auth = req
                        .headers()
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string);
                    let line = format!("{} {}", req.method(), req.uri().path());
                    seen.lock().unwrap().push((line.clone(), auth));
                    if line == "GET /health" {
                        axum::Json(json!({"ready": true, "boot_id": "b1"})).into_response()
                    } else {
                        "ok".into_response()
                    }
                },
            )
            .with_state(seen.clone());
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        Renderer { addr, seen }
    }
    use axum::response::IntoResponse;

    struct Loop(std::net::SocketAddr);

    #[async_trait]
    impl Local for Loop {
        async fn connect(&self, _port: Port) -> Result<Stream> {
            Ok(Box::new(tokio::net::TcpStream::connect(self.0).await?))
        }
    }

    /// Lambda stand-in: each publish starts an "environment" that dials the tunnel with the
    /// configuration it was given, on the instance the test names next.
    struct Fake {
        ep: Arc<Endpoint>,
        renderer: std::net::SocketAddr,
        vars: Mutex<HashMap<String, BTreeMap<String, String>>>,
        instance: Mutex<(String, u64)>,
        publishes: Mutex<Vec<String>>,
        deleted: Mutex<Vec<String>>,
        stops: Mutex<Vec<watch::Sender<bool>>>,
        /// While true, a publish starts no environment (its readiness never comes).
        withhold: std::sync::atomic::AtomicBool,
        /// A configuration waits for this before Lambda takes it.
        put_gate: Option<Arc<tokio::sync::Semaphore>>,
    }

    #[async_trait]
    impl Functions for Fake {
        async fn put(&self, name: &str, _c: Value, vars: &BTreeMap<String, String>) -> Result<()> {
            if let Some(g) = &self.put_gate {
                g.acquire().await.unwrap().forget();
            }
            self.vars.lock().unwrap().insert(name.into(), vars.clone());
            Ok(())
        }
        async fn publish(&self, name: &str) -> Result<()> {
            self.publishes.lock().unwrap().push(name.into());
            if self.withhold.load(std::sync::atomic::Ordering::SeqCst) {
                return Ok(());
            }
            let v = self.vars.lock().unwrap()[name].clone();
            let (instance, boot) = self.instance.lock().unwrap().clone();
            let id = Identity {
                slot: v["FLEET_TILE_SLOT"].clone(),
                generation: v["FLEET_TILE_GENERATION"].parse().unwrap(),
                env: format!("env-{}", v["FLEET_TILE_LAUNCH"]),
                started_ms: crate::fleet::now_ms(),
                credential: v["FLEET_TILE_CREDENTIAL"].clone(),
                instance: Some(instance),
                instance_boot_ms: Some(boot),
            };
            let (up, _) = watch::channel(false);
            let (stop, stop_rx) = watch::channel(false);
            self.stops.lock().unwrap().push(stop);
            tokio::spawn(tunnel::run_client(
                id,
                Arc::new(InProcess(self.ep.clone())),
                Arc::new(Loop(self.renderer)),
                up,
                stop_rx,
            ));
            Ok(())
        }
        async fn delete(&self, name: &str) -> Result<bool> {
            self.deleted.lock().unwrap().push(name.into());
            Ok(self.vars.lock().unwrap().remove(name).is_some())
        }
        async fn published_state(&self, _name: &str) -> Result<(String, Option<String>)> {
            Ok(("Active".into(), None))
        }
    }

    fn driver(fake: Arc<Fake>, ep: Arc<Endpoint>) -> LambdaDriver {
        LambdaDriver::new(
            plan(),
            fake,
            ep,
            tunnel::derive_key("token-for-tests"),
            super::super::scene_token::SceneToken::new(Some("scene-token".into())),
        )
        .with_ready_timeout(Duration::from_secs(10))
    }

    #[tokio::test]
    async fn a_launch_is_ready_once_its_environment_dials_and_its_renderer_is_ready() {
        let ep = Endpoint::new(tunnel::derive_key("token-for-tests"));
        let r = renderer().await;
        let fake = Arc::new(Fake {
            ep: ep.clone(),
            renderer: r.addr,
            vars: Mutex::default(),
            instance: Mutex::new(("i-new".into(), crate::fleet::now_ms() + 60_000)),
            publishes: Mutex::default(),
            deleted: Mutex::default(),
            stops: Mutex::default(),
            withhold: Default::default(),
            put_gate: None,
        });
        let d = driver(fake.clone(), ep.clone());
        // A wake: the first launch lands on an instance Lambda launched after the publish.
        let l = d
            .launch_generation("lam-1", 1)
            .await
            .map_err(|_| "launch")
            .unwrap();
        assert_eq!(l.instance, "fcvm-lam-test-1#1");
        assert_eq!(l.frame_url.as_deref(), Some("wss://lam-1.x.example/frames"));
        assert_eq!(l.ipv6.as_deref(), Some("2001:db8::1"));
        let l = d.ready("lam-1", &l).await.unwrap();
        let t = d.launch_timings("lam-1", &l);
        assert_eq!(t["new_instance"], 1);
        assert!(t["capacity_wait_ms"] > 50_000, "{t:?}");
        // Posts go over the tunnel to the renderer, with the scene token.
        let body = BudgetBody {
            budget: 1,
            t: 0.0,
            every_pixel: false,
        };
        d.post_budget("lam-1", &l, &body).await.unwrap();
        assert!(r
            .seen
            .lock()
            .unwrap()
            .iter()
            .any(|(line, auth)| line == "POST /budget"
                && auth.as_deref() == Some("Bearer scene-token")));
        assert_eq!(d.alive("lam-1", &l).await.unwrap(), Liveness::Alive);
        // A race: generation 2 lands on the instance the tile already had.
        let l2 = d
            .launch_generation("lam-1", 2)
            .await
            .map_err(|_| "launch")
            .unwrap();
        // The old environment's tunnel is no longer the tile's.
        assert!(matches!(
            d.alive("lam-1", &l).await.unwrap(),
            Liveness::Silent(_)
        ));
        let l2 = d.ready("lam-1", &l2).await.unwrap();
        assert_eq!(
            d.launch_timings("lam-1", &l2),
            BTreeMap::from([("new_instance".into(), 0)])
        );
        assert_eq!(fake.publishes.lock().unwrap().len(), 2);
        // The fleet sleeps: the function is deleted and its tunnel ends.
        d.release(&l2.instance).await.unwrap();
        assert_eq!(fake.deleted.lock().unwrap().as_slice(), ["fcvm-lam-test-1"]);
        assert!(ep.sessions().borrow().get("lam-1").is_none());
        for s in fake.stops.lock().unwrap().iter() {
            s.send_replace(true);
        }
    }

    #[tokio::test]
    async fn a_relaunch_of_the_same_generation_starts_a_new_environment() {
        let ep = Endpoint::new(tunnel::derive_key("token-for-tests"));
        let r = renderer().await;
        let fake = Arc::new(Fake {
            ep: ep.clone(),
            renderer: r.addr,
            vars: Mutex::default(),
            instance: Mutex::new(("i-a".into(), 1)),
            publishes: Mutex::default(),
            deleted: Mutex::default(),
            stops: Mutex::default(),
            withhold: Default::default(),
            put_gate: None,
        });
        let d = driver(fake.clone(), ep.clone());
        let l = d
            .launch_generation("lam-3", 4)
            .await
            .map_err(|_| "launch")
            .unwrap();
        d.ready("lam-3", &l).await.unwrap();
        let first = ep.sessions().borrow()["lam-3"].env.clone();
        let l = d
            .launch_generation("lam-3", 4)
            .await
            .map_err(|_| "launch")
            .unwrap();
        d.ready("lam-3", &l).await.unwrap();
        let mut rx = ep.sessions();
        let second = tokio::time::timeout(
            Duration::from_secs(10),
            rx.wait_for(|m| m.get("lam-3").is_some_and(|s| s.env != first)),
        )
        .await
        .expect("the new environment holds the tile")
        .unwrap()["lam-3"]
            .env
            .clone();
        assert_ne!(first, second);
        for s in fake.stops.lock().unwrap().iter() {
            s.send_replace(true);
        }
    }

    #[tokio::test]
    async fn the_sweep_deletes_every_function_of_this_fleets_names() {
        let ep = Endpoint::new(tunnel::derive_key("token-for-tests"));
        let fake = Arc::new(Fake {
            ep: ep.clone(),
            renderer: "127.0.0.1:9".parse().unwrap(),
            vars: Mutex::new(HashMap::from([
                ("fcvm-lam-test-2".to_string(), BTreeMap::new()),
                ("fcvm-lam-test-40".to_string(), BTreeMap::new()),
                ("fcvm-lam-other-1".to_string(), BTreeMap::new()),
            ])),
            instance: Mutex::new(("i".into(), 0)),
            publishes: Mutex::default(),
            deleted: Mutex::default(),
            stops: Mutex::default(),
            withhold: Default::default(),
            put_gate: None,
        });
        let d = driver(fake.clone(), ep);
        assert_eq!(d.sweep().await.unwrap(), 2);
        assert_eq!(fake.deleted.lock().unwrap().len(), MAX_TILES as usize);
        assert!(fake.vars.lock().unwrap().contains_key("fcvm-lam-other-1"));
    }

    /// A handle whose actor only answers, for the reconciler's reports.
    fn quiet_handle() -> super::super::Handle {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<super::super::Cmd>(4096);
        tokio::spawn(async move {
            while let Some(cmd) = rx.recv().await {
                if let super::super::Cmd::Apply {
                    reply: Some(reply), ..
                } = cmd
                {
                    let _ = reply.send(None);
                }
            }
        });
        super::super::Handle {
            cmds: tx,
            snapshot: watch::channel(Arc::new(super::super::Snapshot {
                version: 0,
                text: String::new(),
            }))
            .1,
            lambda: watch::channel(Vec::new()).1,
            ec2_layout: watch::channel(None).1,
            broadcast_layout: watch::channel(None).1,
        }
    }

    fn slot(id: &str, generation: u64) -> fleet_core::protocol::AssignSlot {
        serde_json::from_value(json!({
            "id": id, "sbdf": "", "generation": generation,
            "rect": {"x": 0.5, "y": 0.0, "w": 0.5, "h": 1.0}
        }))
        .unwrap()
    }

    /// The Lambda tiles' reconciler over a stand-in Lambda: the fake, its assignment, and the
    /// renderer's address.
    async fn lane(
        withhold: bool,
        put_gate: Option<Arc<tokio::sync::Semaphore>>,
        ready_timeout: Duration,
    ) -> (
        Arc<Fake>,
        Arc<LambdaDriver>,
        watch::Sender<Vec<fleet_core::protocol::AssignSlot>>,
        Arc<Endpoint>,
    ) {
        let ep = Endpoint::new(tunnel::derive_key("token-for-tests"));
        let r = renderer().await;
        let fake = Arc::new(Fake {
            ep: ep.clone(),
            renderer: r.addr,
            vars: Mutex::default(),
            instance: Mutex::new(("i-a".into(), 1)),
            publishes: Mutex::default(),
            deleted: Mutex::default(),
            stops: Mutex::default(),
            withhold: std::sync::atomic::AtomicBool::new(withhold),
            put_gate,
        });
        let d = Arc::new(driver(fake.clone(), ep.clone()).with_ready_timeout(ready_timeout));
        let (assign_tx, assign_rx) = watch::channel(Vec::new());
        let (_scene_tx, scene_rx) = watch::channel(None);
        let (_budget_tx, budget_rx) = watch::channel(None);
        std::mem::forget((_scene_tx, _budget_tx));
        let driver: Arc<dyn Ec2Driver> = d.clone();
        tokio::spawn(super::super::ec2::run(
            driver,
            assign_rx,
            scene_rx,
            budget_rx,
            quiet_handle(),
        ));
        (fake, d, assign_tx, ep)
    }

    async fn until(what: &str, mut ok: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !ok() {
            assert!(Instant::now() < deadline, "{what}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn deletes_after_sweep(fake: &Fake) -> usize {
        // The start sweep deletes every name once.
        fake.deleted.lock().unwrap().len() - MAX_TILES as usize
    }

    fn stop_all(fake: &Fake) {
        for s in fake.stops.lock().unwrap().iter() {
            s.send_replace(true);
        }
    }

    /// The tile leaves the assignment while it runs: its function is deleted.
    #[tokio::test]
    async fn a_running_tile_that_leaves_has_its_function_deleted() {
        let (fake, _d, assign, ep) = lane(false, None, Duration::from_secs(10)).await;
        assign.send(vec![slot("lam-1", 1)]).unwrap();
        until("the tile runs", || {
            ep.sessions().borrow().contains_key("lam-1")
        })
        .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assign.send(Vec::new()).unwrap();
        until("its function deleted", || deletes_after_sweep(&fake) == 1).await;
        stop_all(&fake);
    }

    /// The tile leaves and comes back while its first task waits for its environment: the
    /// returning task's launch runs, and the first task's late cleanup must not delete it.
    #[tokio::test]
    async fn a_retired_task_does_not_delete_the_launch_of_the_task_that_replaced_it() {
        let (fake, _d, assign, ep) = lane(true, None, Duration::from_secs(10)).await;
        assign.send(vec![slot("lam-1", 1)]).unwrap();
        until("the first launch published", || {
            fake.publishes.lock().unwrap().len() == 1
        })
        .await;
        // Leave, then come back, while the first task still waits for its environment.
        assign.send(Vec::new()).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        fake.withhold
            .store(false, std::sync::atomic::Ordering::SeqCst);
        assign.send(vec![slot("lam-1", 1)]).unwrap();
        until("the returning task published", || {
            fake.publishes.lock().unwrap().len() == 2
        })
        .await;
        until("the returning task's environment holds the tile", || {
            ep.sessions().borrow().contains_key("lam-1")
        })
        .await;
        // Both tasks see that environment ready; the first one then ends.
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(
            deletes_after_sweep(&fake),
            0,
            "the retired task deleted the returning task's function"
        );
        assert!(
            ep.sessions().borrow().contains_key("lam-1"),
            "the retired task ended the returning task's tunnel"
        );
        stop_all(&fake);
    }

    /// The tile leaves while its launch waits for its environment, which then never comes:
    /// the launch never served, and its function is deleted anyway.
    #[tokio::test]
    async fn a_launch_cancelled_while_it_starts_has_its_function_deleted() {
        let (fake, _d, assign, _ep) = lane(true, None, Duration::from_secs(1)).await;
        assign.send(vec![slot("lam-1", 1)]).unwrap();
        until("published", || fake.publishes.lock().unwrap().len() == 1).await;
        assign.send(Vec::new()).unwrap();
        until("the abandoned launch's function deleted", || {
            deletes_after_sweep(&fake) == 1
        })
        .await;
    }

    /// The tile leaves during the backoff after its launch failed to become ready: the
    /// function of that launch is deleted.
    #[tokio::test]
    async fn a_tile_that_leaves_during_the_backoff_has_its_function_deleted() {
        let (fake, _d, assign, _ep) = lane(true, None, Duration::from_millis(300)).await;
        assign.send(vec![slot("lam-1", 1)]).unwrap();
        until("published", || fake.publishes.lock().unwrap().len() == 1).await;
        // Past the readiness timeout: the task reports the failure and backs off.
        tokio::time::sleep(Duration::from_millis(600)).await;
        assign.send(Vec::new()).unwrap();
        until("the failed launch's function deleted", || {
            deletes_after_sweep(&fake) == 1
        })
        .await;
    }

    /// The fleet sleeps while a launch is still configuring its function: once Lambda takes
    /// the configuration the launch publishes, and its function is deleted after it.
    #[tokio::test]
    async fn a_launch_the_fleet_slept_during_has_its_function_deleted() {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let (fake, _d, assign, _ep) = lane(true, Some(gate.clone()), Duration::from_secs(1)).await;
        assign.send(vec![slot("lam-1", 1)]).unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assign.send(Vec::new()).unwrap();
        gate.add_permits(1);
        until("published", || fake.publishes.lock().unwrap().len() == 1).await;
        until("the slept launch's function deleted", || {
            deletes_after_sweep(&fake) == 1
        })
        .await;
    }

    /// The coordinator's shutdown sweep deletes every function and refuses any launch after
    /// it, so no function is left published behind a stopped coordinator.
    #[tokio::test]
    async fn the_shutdown_sweep_refuses_later_launches() {
        let (fake, d, assign, ep) = lane(false, None, Duration::from_secs(10)).await;
        assign.send(vec![slot("lam-1", 1)]).unwrap();
        until("the tile runs", || {
            ep.sessions().borrow().contains_key("lam-1")
        })
        .await;
        assert_eq!(d.sweep_at_shutdown().await.unwrap(), 1);
        assert!(
            d.launch_owned("lam-1", 2, 1).await.is_err(),
            "a launch after the shutdown sweep"
        );
        // A tile task that starts after it cannot take the tile either.
        d.claim("lam-1", 7).await;
        assert!(
            d.launch_owned("lam-1", 2, 7).await.is_err(),
            "a launch by a task that claimed the tile after the shutdown sweep"
        );
        // The reconciler's task for the tile cannot launch it again either.
        assign.send(vec![slot("lam-1", 2)]).unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(fake.publishes.lock().unwrap().len(), 1);
        stop_all(&fake);
    }
}
