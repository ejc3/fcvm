//! `fcvm fleet agent`: runs one fcvm GPU VM per assigned slot on this host and reports
//! to the coordinator over newline-delimited JSON on stdin and stdout.

pub mod addr;
pub mod dial;
pub mod inventory;
pub mod slot;
pub mod spot;
pub mod tls;
pub mod vm;

use crate::cli::FleetAgentArgs;
use crate::fleet::aggregate::{Layout, TileSpec};
use crate::fleet::lines::{spawn_writer, LineReader};
use anyhow::{Context, Result};
use fleet_core::protocol::{AgentMsg, AssignSlot, BudgetBody, CoordMsg, SharedScene};
use fleet_core::state::Gpu;
use slot::{supervise, AgentCfg, Desired, SlotCtx};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, watch, Mutex};
use tokio::task::JoinHandle;

pub struct Agent {
    pub host: String,
    pub gpus: Vec<Gpu>,
    pub capacity: usize,
    pub base_port: u16,
    pub cfg: AgentCfg,
    pub spot_notice_url: Option<String>,
    /// How often the spot notice URL is polled.
    pub spot_poll: Duration,
    /// Run an aggregator for this host's band on this port, with this command
    /// (`fcvm fleet aggregate ...`; `--listen` is added).
    pub aggregate: Option<(u16, Vec<String>)>,
    /// A file the host's boot writes once its store is mounted; its modification time is
    /// reported in the hello.
    pub store_ready_file: Option<std::path::PathBuf>,
}

/// The modification time of `path` in Unix milliseconds, if it exists.
fn mtime_ms(path: &std::path::Path) -> Option<u64> {
    let t = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(t.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis() as u64)
}

pub async fn run(args: FleetAgentArgs) -> Result<()> {
    let host = match args.host {
        Some(h) => h,
        None if args.host_from_imds => crate::fleet::coord::aws::imds("instance-id")
            .await
            .context("reading the instance id")?,
        None => std::fs::read_to_string("/proc/sys/kernel/hostname")
            .context("reading the host name")?
            .trim()
            .to_string(),
    };
    let gpus = match &args.gpus {
        Some(spec) => inventory::parse_gpus(spec)?,
        None => inventory::vfio_gpus(&args.sysfs_root)?,
    };
    if gpus.is_empty() {
        tracing::warn!("no NVIDIA GPU is bound to vfio-pci; this host will run no slot");
    }
    // The renderers' certificate: fetched once before any VM starts, then kept current.
    let mut fcvm_args = args.fcvm_args.clone();
    if let Some(secret) = &args.tls_secret {
        let region = match &args.secret_region {
            Some(r) => r.clone(),
            None => crate::fleet::coord::aws::imds("placement/region")
                .await
                .context("reading the instance's region (or pass --secret-region)")?,
        };
        let secrets = crate::fleet::coord::secrets::Secrets::new(&region)?;
        tls::refresh(&secrets, secret, &args.tls_dir).await?;
        fcvm_args.extend(tls::vm_args(&args.tls_dir));
        tokio::spawn(tls::run(secrets, secret.clone(), args.tls_dir.clone()));
    }
    let ipv6_prefix = match (&args.ipv6_prefix, args.ipv6_prefix_from_imds) {
        (Some(p), _) => Some(addr::Prefix::parse(p)?),
        (None, true) => Some(addr::from_imds().await?),
        (None, false) => None,
    };
    addr::check_routed_user(ipv6_prefix.is_some(), nix::unistd::geteuid().is_root())?;
    let fcvm = match args.fcvm {
        Some(p) => p,
        None => std::env::current_exe().context("locating this fcvm binary")?,
    };
    let agent = Agent {
        host,
        capacity: args.capacity.unwrap_or(gpus.len()),
        gpus,
        base_port: args.base_port,
        cfg: AgentCfg {
            fcvm,
            image: args.image,
            extra_args: fcvm_args,
            frame_host: args.frame_host.unwrap_or_else(|| args.publish_ip.clone()),
            ipv6_prefix,
            frame_zone: args.frame_zone,
            publish_ip: args.publish_ip,
            sysfs_root: args.sysfs_root,
            start_timeout: Duration::from_secs(args.start_timeout_secs),
            stop_grace: Duration::from_secs(30),
            gpu_free_timeout: Duration::from_secs(30),
            health_interval: Duration::from_secs(1),
            backoff_min: Duration::from_secs(1),
            backoff_max: Duration::from_secs(30),
            stable_after: Duration::from_secs(60),
            scrub: args.scrub,
            scrub_timeout: Duration::from_secs(120),
            quarantine_after: 3,
        },
        spot_notice_url: args.spot_notice_url,
        spot_poll: Duration::from_secs(5),
        store_ready_file: args.store_ready_file.clone(),
        aggregate: args.aggregate_port.map(|p| {
            let mut argv = vec![
                std::env::current_exe()
                    .map(|e| e.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| "fcvm".into()),
                "fleet".into(),
                "aggregate".into(),
            ];
            argv.extend(args.aggregate_args.iter().cloned());
            (p, argv)
        }),
    };
    let Some(url) = args.dial else {
        return serve(agent, tokio::io::stdin(), tokio::io::stdout()).await;
    };
    let mut headers = Vec::new();
    if let Some(secret) = &args.access_secret {
        let region = match &args.secret_region {
            Some(r) => r.clone(),
            None => crate::fleet::coord::aws::imds("placement/region")
                .await
                .context("reading the instance's region (or pass --secret-region)")?,
        };
        let v: serde_json::Value = serde_json::from_str(
            &crate::fleet::coord::secrets::Secrets::new(&region)?
                .get(secret)
                .await?,
        )
        .with_context(|| format!("secret {secret} is not JSON"))?;
        let field = |k: &str| {
            v.get(k)
                .and_then(|x| x.as_str())
                .map(str::to_string)
                .with_context(|| format!("secret {secret} has no {k}"))
        };
        headers.push(("CF-Access-Client-Id".to_string(), field("client_id")?));
        headers.push((
            "CF-Access-Client-Secret".to_string(),
            field("client_secret")?,
        ));
    }
    dial::run(
        agent,
        dial::DialCfg {
            url,
            headers,
            backoff_min: Duration::from_secs(1),
            backoff_max: Duration::from_secs(30),
        },
    )
    .await
}

struct SlotHandle {
    sbdf: String,
    desired: watch::Sender<Desired>,
    task: JoinHandle<()>,
}

/// Run the agent over one connection until the coordinator closes it, then stop every VM.
pub async fn serve<R, W>(agent: Agent, input: R, output: W) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let mut ports = HashMap::new();
    for (i, g) in agent.gpus.iter().enumerate() {
        let port = u16::try_from(i)
            .ok()
            .and_then(|i| agent.base_port.checked_add(i))
            .with_context(|| {
                format!("--base-port {} leaves no port for GPU {i}", agent.base_port)
            })?;
        let addr = match &agent.cfg.ipv6_prefix {
            Some(p) => Some(p.slot_address(i)?),
            None => None,
        };
        ports.insert(g.sbdf.clone(), (port, addr));
    }
    let (out, writer) = spawn_writer::<AgentMsg, _>(output, "coordinator");
    out.send(AgentMsg::Hello {
        host: agent.host.clone(),
        arch: std::env::consts::ARCH.to_string(),
        gpus: agent.gpus.clone(),
        capacity: agent.capacity,
        store_ready_at: agent.store_ready_file.as_deref().and_then(mtime_ms),
    })
    .ok();
    let http = reqwest::Client::builder()
        .no_proxy()
        .build()
        .context("building the HTTP client")?;
    let spot_poll = agent.spot_poll;
    let spot = agent.spot_notice_url.clone().map(|url| {
        let out = out.clone();
        let http = http.clone();
        tokio::spawn(async move {
            let body = spot::wait_for_notice(&http, &url, spot_poll).await;
            tracing::warn!(notice = %body, "spot interruption notice");
            let _ = out.send(AgentMsg::Draining {
                reason: format!("spot interruption notice: {}", body.trim()),
            });
        })
    });

    let cfg = Arc::new(agent.cfg);
    let (scene_tx, scene_rx) = watch::channel::<Option<SharedScene>>(None);
    let (budget_tx, budget_rx) = watch::channel::<Option<BudgetBody>>(None);
    // The aggregator's layout follows the assignment and the scene.
    let (layout_tx, layout_rx) = watch::channel::<Option<Layout>>(None);
    let aggregator = agent.aggregate.clone().map(|(port, argv)| {
        let listen = format!("{}:{port}", cfg.publish_ip);
        let url = slot::frame_url(&cfg.frame_host, port);
        let out = out.clone();
        tokio::spawn(crate::fleet::aggregate::child::supervise(
            argv,
            listen,
            layout_rx,
            move || {
                let _ = out.send(AgentMsg::Aggregate {
                    frame_url: url.clone(),
                });
            },
        ))
    });
    let mut region: Option<fleet_core::tiling::Rect> = None;
    let mut placed: Vec<AssignSlot> = Vec::new();
    let gpu_locks: HashMap<String, Arc<Mutex<()>>> = agent
        .gpus
        .iter()
        .map(|g| (g.sbdf.clone(), Arc::new(Mutex::new(()))))
        .collect();
    let mut slots: BTreeMap<String, SlotHandle> = BTreeMap::new();
    let mut retired: Vec<JoinHandle<()>> = Vec::new();
    let mut reader = LineReader::new(input, "coordinator");

    loop {
        let msg = match reader.next::<CoordMsg>().await {
            Ok(Some(m)) => m,
            Ok(None) => break,
            Err(e) => {
                tracing::warn!(error = %e, "coordinator connection failed");
                break;
            }
        };
        match msg {
            CoordMsg::Scene(s) => {
                scene_tx.send_replace(Some(s));
            }
            CoordMsg::Budget(b) => {
                budget_tx.send_replace(Some(b));
            }
            CoordMsg::Assign {
                slots: list,
                region: r,
            } => {
                region = r;
                placed = list.iter().filter(|a| a.rect.is_some()).cloned().collect();
                assign(
                    &list,
                    &mut slots,
                    &mut retired,
                    &ports,
                    &gpu_locks,
                    &cfg,
                    &out,
                    &http,
                    &scene_rx,
                    &budget_rx,
                );
                tracing::debug!(slots = list.len(), "assignment applied");
            }
        }
        let layout = match (region, scene_rx.borrow().as_ref()) {
            (Some(region), Some(scene)) => Some(Layout {
                epoch_unix_ms: scene.epoch_unix_ms,
                image_aspect: scene.image_aspect,
                region,
                tiles: placed
                    .iter()
                    .filter_map(|a| {
                        Some(TileSpec {
                            id: a.id.clone(),
                            rect: a.rect?,
                            url: ports
                                .get(&a.sbdf)
                                .map(|(p, _)| format!("ws://{}:{p}/frames", cfg.health_host())),
                        })
                    })
                    .collect(),
                generation: placed
                    .iter()
                    .map(|a| a.layout_generation)
                    .max()
                    .unwrap_or(0),
                from_frame: placed
                    .iter()
                    .max_by_key(|a| a.layout_generation)
                    .map_or(0, |a| a.rect_from_frame),
            }),
            _ => None,
        };
        layout_tx.send_if_modified(|cur| {
            let changed = *cur != layout;
            *cur = layout.clone();
            changed
        });
    }
    if let Some(a) = aggregator {
        a.abort();
    }

    tracing::info!(
        slots = slots.len(),
        "coordinator closed the connection; stopping every VM"
    );
    if let Some(s) = spot {
        s.abort();
    }
    for (_, h) in std::mem::take(&mut slots) {
        drop(h.desired);
        retired.push(h.task);
    }
    for t in retired {
        let _ = t.await;
    }
    drop(out);
    let _ = writer.await;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn assign(
    list: &[AssignSlot],
    slots: &mut BTreeMap<String, SlotHandle>,
    retired: &mut Vec<JoinHandle<()>>,
    ports: &HashMap<String, (u16, Option<std::net::Ipv6Addr>)>,
    gpu_locks: &HashMap<String, Arc<Mutex<()>>>,
    cfg: &Arc<AgentCfg>,
    out: &mpsc::UnboundedSender<AgentMsg>,
    http: &reqwest::Client,
    scene_rx: &watch::Receiver<Option<SharedScene>>,
    budget_rx: &watch::Receiver<Option<BudgetBody>>,
) {
    // Slots no longer listed, or moved to another GPU, are torn down by their tasks.
    let keep: HashMap<&str, &str> = list
        .iter()
        .map(|a| (a.id.as_str(), a.sbdf.as_str()))
        .collect();
    let gone: Vec<String> = slots
        .iter()
        .filter(|(id, h)| keep.get(id.as_str()) != Some(&h.sbdf.as_str()))
        .map(|(id, _)| id.clone())
        .collect();
    for id in gone {
        let h = slots.remove(&id).unwrap();
        drop(h.desired);
        retired.push(h.task);
    }
    retired.retain(|t| !t.is_finished());
    for a in list {
        let want = Desired {
            generation: a.generation,
            rect: a.rect,
            private_camera: a.private_camera.clone(),
            standby: a.standby,
            layout_generation: a.layout_generation,
            rect_from_frame: a.rect_from_frame,
            previous_rect: a.previous_rect,
        };
        if let Some(h) = slots.get(&a.id) {
            h.desired.send_if_modified(|d| {
                // Generations only grow; an older assignment never moves a slot back.
                let next = Desired {
                    generation: d.generation.max(want.generation),
                    rect: want.rect,
                    private_camera: want.private_camera.clone(),
                    standby: want.standby,
                    layout_generation: want.layout_generation,
                    rect_from_frame: want.rect_from_frame,
                    previous_rect: want.previous_rect,
                };
                let changed = *d != next;
                *d = next;
                changed
            });
            continue;
        }
        let (Some(&(port, ipv6)), Some(lock)) = (ports.get(&a.sbdf), gpu_locks.get(&a.sbdf)) else {
            tracing::warn!(slot = %a.id, sbdf = %a.sbdf, "assigned a GPU this host does not have");
            let _ = out.send(AgentMsg::Slot(fleet_core::protocol::SlotReport {
                id: a.id.clone(),
                phase: "failed".into(),
                generation: a.generation,
                vm: None,
                frame_url: None,
                timings: Default::default(),
                error: Some(format!("GPU {} is not on this host", a.sbdf)),
                retry_in_ms: None,
                ipv6: None,
                ipv4: None,
            }));
            continue;
        };
        let (tx, rx) = watch::channel(want);
        let ctx = SlotCtx {
            id: a.id.clone(),
            sbdf: a.sbdf.clone(),
            port,
            ipv6,
            cfg: cfg.clone(),
            out: out.clone(),
            gpu_lock: lock.clone(),
            http: http.clone(),
        };
        let task = tokio::spawn(supervise(ctx, rx, scene_rx.clone(), budget_rx.clone()));
        slots.insert(
            a.id.clone(),
            SlotHandle {
                sbdf: a.sbdf.clone(),
                desired: tx,
                task,
            },
        );
    }
}
