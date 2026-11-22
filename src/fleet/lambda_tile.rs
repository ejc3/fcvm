//! `fcvm fleet lambda-tile`: a Lambda tile's runtime (fleet/PROTOCOL.md, "Lambda tile").
//!
//! It is the Lambda custom runtime of the lane image (`fleet/lambda/`). In the execution
//! environment's init it writes the fleet's certificate and scene token where the renderer
//! reads them, starts the renderer (HTTP on 127.0.0.1, TLS on its own port), and dials the
//! coordinator's tunnel endpoint, which carries every viewer, scene and health stream to the
//! renderer ([`crate::fleet::tunnel`]). Then it serves invocations: `health` answers the
//! renderer's `GET /health`, and `restart` restarts the renderer process in this environment
//! and times it to its first ready health (the separate warm field; a race publishes a new
//! version instead, which is a new environment).
//!
//! The environment only dials out. Nothing in it listens on an address another machine can
//! reach.

use super::coord::secrets::Secrets;
use super::tunnel::{self, Dialer, Ended, Identity, Local, Port, Stream};
use crate::cli::FleetLambdaTileArgs;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{watch, Mutex};

/// How long a renderer may take to answer `/health` with `ready` after it starts.
const READY_TIMEOUT: Duration = Duration::from_secs(120);
/// How long a renderer that exited waits before it is started again.
const RESTART_BACKOFF: Duration = Duration::from_secs(2);

pub async fn run(args: FleetLambdaTileArgs) -> Result<()> {
    let api = std::env::var("AWS_LAMBDA_RUNTIME_API")
        .context("AWS_LAMBDA_RUNTIME_API is not set: this runs as a Lambda runtime")?;
    let runtime = Runtime::new(&api)?;
    match init(&args).await {
        Ok(tile) => {
            let tile = Arc::new(tile);
            runtime
                .serve(args.workers, move |event| {
                    let tile = tile.clone();
                    async move { tile.invoke(event).await }
                })
                .await
        }
        Err(e) => {
            runtime.init_error(&e).await;
            Err(e)
        }
    }
}

/// The environment as the tunnel names it: a random id and when it started.
fn environment() -> (String, u64) {
    let mut b = [0u8; 6];
    rand::Rng::fill(&mut rand::rng(), &mut b[..]);
    (hex::encode(b), super::now_ms())
}

/// The kernel's boot time (Unix ms) from `/proc/stat`'s `btime` line.
pub fn boot_ms(proc_stat: &str) -> Option<u64> {
    proc_stat
        .lines()
        .find_map(|l| l.strip_prefix("btime "))
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|s| s * 1000)
}

/// A short stable name for a value read from the machine (never the value itself).
pub fn short_hash(v: &str) -> String {
    hex::encode(&ring::digest::digest(&ring::digest::SHA256, v.trim().as_bytes()).as_ref()[..6])
}

/// The managed instance this environment runs on: a hash of the DMI board asset tag (the
/// instance id on Nitro) and the instance's boot time. Environments on one instance share both.
fn instance() -> (Option<String>, Option<u64>) {
    let asset = std::fs::read_to_string("/sys/class/dmi/id/board_asset_tag")
        .ok()
        .filter(|a| !a.trim().is_empty())
        .map(|a| short_hash(&a));
    let boot = std::fs::read_to_string("/proc/stat")
        .ok()
        .and_then(|s| boot_ms(&s));
    (asset, boot)
}

/// Write the certificate secret's `cert` and `key` as the renderer's `TLS_CERT_FILE` (JSON
/// `{cert, key}`) and its `scene_token` as `SCENE_TOKEN_FILE`, each readable by this user
/// alone. Returns the two paths.
pub fn write_identity(dir: &Path, secret: &str) -> Result<(PathBuf, PathBuf)> {
    let doc: Value = serde_json::from_str(secret).context("the certificate secret is not JSON")?;
    let field = |k: &str| -> Result<&str> {
        doc.get(k)
            .and_then(|v| v.as_str())
            .filter(|v| !v.is_empty())
            .with_context(|| format!("the certificate secret has no {k}"))
    };
    let tls = json!({"cert": field("cert")?, "key": field("key")?}).to_string();
    let token = field("scene_token")?;
    std::fs::create_dir_all(dir)?;
    let tls_path = dir.join("tls.json");
    let token_path = dir.join("scene-token");
    for (path, text) in [(&tls_path, tls.as_str()), (&token_path, token)] {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let tmp = path.with_extension("tmp");
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(text.as_bytes())?;
        std::fs::rename(&tmp, path)?;
    }
    Ok((tls_path, token_path))
}

/// The renderer process, started again when it exits.
struct Renderer {
    cmd: Vec<String>,
    envs: Vec<(String, String)>,
    child: Mutex<Option<tokio::process::Child>>,
}

impl Renderer {
    async fn start(&self) -> Result<()> {
        let mut c = tokio::process::Command::new(&self.cmd[0]);
        c.args(&self.cmd[1..])
            .envs(self.envs.iter().cloned())
            .kill_on_drop(true);
        let child = c
            .spawn()
            .with_context(|| format!("starting {}", self.cmd[0]))?;
        *self.child.lock().await = Some(child);
        Ok(())
    }

    /// Start it again whenever it exits, as the EC2 tile's unit does (`Restart=always`).
    async fn supervise(self: Arc<Self>) {
        loop {
            let status = {
                let mut guard = self.child.lock().await;
                match guard.as_mut() {
                    // Waiting holds the lock only until the child exits or is taken.
                    Some(c) => match c.try_wait() {
                        Ok(Some(s)) => Some(s),
                        _ => None,
                    },
                    None => None,
                }
            };
            if let Some(s) = status {
                tracing::warn!(%s, "the renderer exited; starting it again");
                tokio::time::sleep(RESTART_BACKOFF).await;
                if let Err(e) = self.start().await {
                    tracing::error!(error = %e, "the renderer did not start");
                }
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    async fn restart(&self) -> Result<()> {
        if let Some(mut c) = self.child.lock().await.take() {
            let _ = c.kill().await;
        }
        self.start().await
    }

    async fn stop(&self) {
        if let Some(mut c) = self.child.lock().await.take() {
            let _ = c.kill().await;
        }
    }
}

struct Tile {
    id: Identity,
    http: reqwest::Client,
    health_url: String,
    renderer: Arc<Renderer>,
    tunnel_up: watch::Receiver<bool>,
    ended: watch::Receiver<Option<Ended>>,
}

impl Tile {
    async fn health(&self) -> Result<Value> {
        let r = self.http.get(&self.health_url).send().await?;
        if !r.status().is_success() {
            bail!("the renderer's /health answered {}", r.status());
        }
        Ok(r.json().await?)
    }

    async fn wait_ready(&self) -> Result<Value> {
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            if let Ok(h) = self.health().await {
                if h.get("ready").and_then(|r| r.as_bool()) == Some(true) {
                    return Ok(h);
                }
            }
            if Instant::now() > deadline {
                bail!("the renderer was not ready within {READY_TIMEOUT:?}");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn about(&self) -> Value {
        json!({
            "slot": self.id.slot,
            "generation": self.id.generation,
            "env": self.id.env,
            "started_ms": self.id.started_ms,
            "instance": self.id.instance,
            "instance_boot_ms": self.id.instance_boot_ms,
            "tunnel_up": *self.tunnel_up.borrow(),
            "tunnel_ended": self.ended.borrow().map(|e| format!("{e:?}")),
        })
    }

    async fn invoke(&self, event: Value) -> Result<Value> {
        let action = event
            .get("action")
            .and_then(|a| a.as_str())
            .unwrap_or("health");
        let t0 = Instant::now();
        let mut out = self.about();
        match action {
            "health" => out["health"] = self.health().await?,
            "restart" => {
                self.renderer.restart().await?;
                out["health"] = self.wait_ready().await?;
                out["restart_ms"] = json!(t0.elapsed().as_millis() as u64);
            }
            other => bail!("unknown action {other:?}"),
        }
        out["ms"] = json!(t0.elapsed().as_millis() as u64);
        Ok(out)
    }
}

/// Dials the endpoint over TLS, verified against the public roots, to `host`.
struct TlsDialer {
    host: String,
    port: u16,
    connector: tokio_rustls::TlsConnector,
}

#[async_trait::async_trait]
impl Dialer for TlsDialer {
    async fn dial(&self) -> Result<Stream> {
        let tcp = tokio::time::timeout(
            Duration::from_secs(10),
            tokio::net::TcpStream::connect((self.host.as_str(), self.port)),
        )
        .await
        .context("dialing the tunnel endpoint timed out")??;
        tcp.set_nodelay(true)?;
        let name = rustls::pki_types::ServerName::try_from(self.host.clone())?;
        let tls = self.connector.connect(name, tcp).await?;
        Ok(Box::new(tls))
    }
}

/// The renderer's two listeners on loopback.
struct Loopback {
    http: u16,
    tls: u16,
}

#[async_trait::async_trait]
impl Local for Loopback {
    async fn connect(&self, port: Port) -> Result<Stream> {
        let p = match port {
            Port::Http => self.http,
            Port::Tls => self.tls,
        };
        let s = tokio::net::TcpStream::connect(("127.0.0.1", p)).await?;
        s.set_nodelay(true)?;
        Ok(Box::new(s))
    }
}

fn connector() -> tokio_rustls::TlsConnector {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("the ring provider supports the default versions")
    .with_root_certificates(roots)
    .with_no_client_auth();
    tokio_rustls::TlsConnector::from(Arc::new(config))
}

/// Split `host:port`, the port defaulting to 443.
pub fn host_port(s: &str) -> Result<(String, u16)> {
    if let Some((h, p)) = s.rsplit_once(':') {
        if let (false, Ok(p)) = (h.is_empty(), p.parse::<u16>()) {
            return Ok((h.to_string(), p));
        }
        bail!("the tunnel endpoint {s:?} is not host:port");
    }
    if s.is_empty() {
        bail!("no tunnel endpoint");
    }
    Ok((s.to_string(), 443))
}

async fn init(args: &FleetLambdaTileArgs) -> Result<Tile> {
    let (env, started_ms) = environment();
    let (inst, boot) = instance();
    let secret = Secrets::new(&args.tls_secret_region)?
        .get(&args.tls_secret)
        .await
        .context("reading the certificate secret")?;
    let (tls_file, token_file) = write_identity(&args.state_dir, &secret)?;
    drop(secret);
    let renderer = Arc::new(Renderer {
        cmd: shell_words::split(&args.renderer).context("--renderer")?,
        envs: vec![
            ("PORT".into(), args.http_port.to_string()),
            ("TLS_PORT".into(), args.tls_port.to_string()),
            ("TLS_CERT_FILE".into(), tls_file.display().to_string()),
            ("SCENE_TOKEN_FILE".into(), token_file.display().to_string()),
        ],
        child: Mutex::new(None),
    });
    if renderer.cmd.is_empty() {
        bail!("--renderer names no program");
    }
    renderer.start().await?;
    tokio::spawn(renderer.clone().supervise());
    let id = Identity {
        slot: args.slot.clone(),
        generation: args.generation,
        env,
        started_ms,
        credential: args.credential.clone(),
        instance: inst,
        instance_boot_ms: boot,
    };
    let (host, port) = host_port(&args.tunnel)?;
    let dialer: Arc<dyn Dialer> = Arc::new(TlsDialer {
        host,
        port,
        connector: connector(),
    });
    let local: Arc<dyn Local> = Arc::new(Loopback {
        http: args.http_port,
        tls: args.tls_port,
    });
    let (up_tx, tunnel_up) = watch::channel(false);
    let (_stop_tx, stop_rx) = watch::channel(false);
    let (ended_tx, ended) = watch::channel(None);
    {
        let (id, renderer) = (id.clone(), renderer.clone());
        tokio::spawn(async move {
            let _keep = _stop_tx;
            let e = tunnel::run_client(id, dialer, local, up_tx, stop_rx).await;
            // This environment no longer holds its tile: free the GPU until Lambda retires it.
            renderer.stop().await;
            ended_tx.send_replace(Some(e));
        });
    }
    tracing::info!(slot = %id.slot, generation = id.generation, env = %id.env, "the Lambda tile started its renderer and its tunnel");
    Ok(Tile {
        id,
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()?,
        health_url: format!("http://127.0.0.1:{}/health", args.http_port),
        renderer,
        tunnel_up,
        ended,
    })
}

/// The Lambda Runtime API (2018-06-01): take an invocation, answer it.
pub struct Runtime {
    base: String,
    http: reqwest::Client,
}

impl Runtime {
    pub fn new(api: &str) -> Result<Self> {
        Ok(Runtime {
            base: format!("http://{api}/2018-06-01/runtime"),
            // `next` blocks until an invocation arrives: no timeout on it.
            http: reqwest::Client::builder().build()?,
        })
    }

    async fn init_error(&self, e: &anyhow::Error) {
        let _ = self
            .http
            .post(format!("{}/init/error", self.base))
            .json(&json!({"errorType": "InitError", "errorMessage": format!("{e:#}")}))
            .send()
            .await;
    }

    /// Serve invocations with `workers` loops polling `next` at once (Managed Instances route
    /// several invocations to one environment). Returns only when the Runtime API fails.
    pub async fn serve<F, Fut>(&self, workers: usize, handler: F) -> Result<()>
    where
        F: Fn(Value) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<Value>> + Send,
    {
        let handler = Arc::new(handler);
        let mut set = tokio::task::JoinSet::new();
        for _ in 0..workers.max(1) {
            let (base, http, handler) = (self.base.clone(), self.http.clone(), handler.clone());
            set.spawn(async move {
                loop {
                    let r = http.get(format!("{base}/invocation/next")).send().await?;
                    if !r.status().is_success() {
                        bail!("the Runtime API's next answered {}", r.status());
                    }
                    let id = r
                        .headers()
                        .get("lambda-runtime-aws-request-id")
                        .and_then(|v| v.to_str().ok())
                        .context("an invocation without a request id")?
                        .to_string();
                    let event: Value = r.json().await.unwrap_or(Value::Null);
                    match handler(event).await {
                        Ok(v) => {
                            http.post(format!("{base}/invocation/{id}/response"))
                                .json(&v)
                                .send()
                                .await?;
                        }
                        Err(e) => {
                            http.post(format!("{base}/invocation/{id}/error"))
                                .json(&json!({"errorType": "InvokeError", "errorMessage": format!("{e:#}")}))
                                .send()
                                .await?;
                        }
                    }
                }
            });
        }
        match set.join_next().await {
            Some(Ok(r)) => r,
            Some(Err(e)) => Err(e.into()),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boot_time_is_read_from_proc_stat() {
        let stat = "cpu  1 2 3\nintr 5\nbtime 1795327000\nprocesses 9\n";
        assert_eq!(boot_ms(stat), Some(1_795_327_000_000));
        assert_eq!(boot_ms("cpu 1\n"), None);
    }

    #[test]
    fn a_machine_value_is_named_by_a_short_hash_not_itself() {
        let h = short_hash("i-0123456789abcdef0\n");
        assert_eq!(h.len(), 12);
        assert_eq!(h, short_hash("i-0123456789abcdef0"));
        assert!(!h.contains("0123456789"));
    }

    #[test]
    fn the_tunnel_endpoint_takes_a_port_or_defaults_to_443() {
        assert_eq!(
            host_port("tun-x.example:8443").unwrap(),
            ("tun-x.example".into(), 8443)
        );
        assert_eq!(
            host_port("tun-x.example").unwrap(),
            ("tun-x.example".into(), 443)
        );
        assert!(host_port("").is_err());
    }

    #[test]
    fn the_certificate_secret_becomes_the_renderers_two_files() {
        let dir = tempfile::tempdir().unwrap();
        let secret =
            json!({"cert": "CERT", "key": "KEY", "scene_token": "TOKEN", "domains": ["*.x"]})
                .to_string();
        let (tls, token) = write_identity(dir.path(), &secret).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&tls).unwrap()).unwrap();
        assert_eq!(v, json!({"cert": "CERT", "key": "KEY"}));
        assert_eq!(std::fs::read_to_string(&token).unwrap(), "TOKEN");
        use std::os::unix::fs::PermissionsExt;
        for p in [&tls, &token] {
            assert_eq!(
                std::fs::metadata(p).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let no_token = json!({"cert": "C", "key": "K"}).to_string();
        let e = write_identity(dir.path(), &no_token).unwrap_err();
        assert!(e.to_string().contains("scene_token"), "{e}");
    }

    /// A Runtime API stand-in: hands out the queued events and records the answers.
    #[tokio::test]
    async fn invocations_are_answered_through_the_runtime_api() {
        use axum::extract::{Path as AxPath, State};
        use axum::routing::{get, post};
        #[derive(Clone, Default)]
        struct Api {
            queue: Arc<std::sync::Mutex<Vec<Value>>>,
            answers: Arc<std::sync::Mutex<Vec<(String, String, Value)>>>,
            next: Arc<tokio::sync::Notify>,
        }
        let api = Api::default();
        api.queue
            .lock()
            .unwrap()
            .extend([json!({"action": "ok"}), json!({"action": "fail"})]);
        let app = axum::Router::new()
            .route(
                "/2018-06-01/runtime/invocation/next",
                get(|State(a): State<Api>| async move {
                    loop {
                        let ev = { a.queue.lock().unwrap().pop() };
                        if let Some(ev) = ev {
                            let n = a.answers.lock().unwrap().len();
                            return (
                                [(
                                    "lambda-runtime-aws-request-id",
                                    format!("req-{}-{n}", ev["action"].as_str().unwrap()),
                                )],
                                axum::Json(ev),
                            );
                        }
                        a.next.notified().await;
                    }
                }),
            )
            .route(
                "/2018-06-01/runtime/invocation/{id}/{kind}",
                post(
                    |State(a): State<Api>,
                     AxPath((id, kind)): AxPath<(String, String)>,
                     axum::Json(v): axum::Json<Value>| async move {
                        a.answers.lock().unwrap().push((id, kind, v));
                        "ok"
                    },
                ),
            )
            .with_state(api.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let rt = Runtime::new(&addr.to_string()).unwrap();
        tokio::spawn(async move {
            rt.serve(2, |ev: Value| async move {
                match ev["action"].as_str() {
                    Some("ok") => Ok(json!({"answer": 42})),
                    _ => bail!("no such action"),
                }
            })
            .await
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        while api.answers.lock().unwrap().len() < 2 {
            assert!(Instant::now() < deadline, "both invocations answered");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let mut got = api.answers.lock().unwrap().clone();
        got.sort_by(|a, b| a.1.cmp(&b.1));
        assert_eq!(got[0].1, "error");
        assert!(got[0].0.starts_with("req-fail"));
        assert_eq!(got[0].2["errorType"], "InvokeError");
        assert_eq!(
            got[1],
            (got[1].0.clone(), "response".into(), json!({"answer": 42}))
        );
        assert!(got[1].0.starts_with("req-ok"));
    }
}
