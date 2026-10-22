//! `fcvm fleet agent`: runs one fcvm GPU VM per assigned slot on this host and reports
//! to the coordinator over newline-delimited JSON on stdin and stdout.

pub mod inventory;
pub mod slot;
pub mod spot;
pub mod vm;

use crate::cli::FleetAgentArgs;
use crate::fleet::lines::{spawn_writer, LineReader};
use anyhow::{Context, Result};
use fleet_core::protocol::{AgentMsg, AssignSlot, CoordMsg, SharedScene};
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
}

pub async fn run(args: FleetAgentArgs) -> Result<()> {
    let host = match args.host {
        Some(h) => h,
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
            extra_args: args.fcvm_args,
            frame_host: args.frame_host.unwrap_or_else(|| args.publish_ip.clone()),
            publish_ip: args.publish_ip,
            sysfs_root: args.sysfs_root,
            start_timeout: Duration::from_secs(args.start_timeout_secs),
            stop_grace: Duration::from_secs(30),
            gpu_free_timeout: Duration::from_secs(30),
            health_interval: Duration::from_secs(1),
            backoff_min: Duration::from_secs(1),
            backoff_max: Duration::from_secs(30),
            stable_after: Duration::from_secs(60),
        },
        spot_notice_url: args.spot_notice_url,
    };
    serve(agent, tokio::io::stdin(), tokio::io::stdout()).await
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
        ports.insert(g.sbdf.clone(), port);
    }
    let (out, writer) = spawn_writer::<AgentMsg, _>(output, "coordinator");
    out.send(AgentMsg::Hello {
        host: agent.host.clone(),
        arch: std::env::consts::ARCH.to_string(),
        gpus: agent.gpus.clone(),
        capacity: agent.capacity,
    })
    .ok();
    let http = reqwest::Client::builder()
        .no_proxy()
        .build()
        .context("building the HTTP client")?;
    let spot = agent.spot_notice_url.clone().map(|url| {
        let out = out.clone();
        let http = http.clone();
        tokio::spawn(async move {
            let body = spot::wait_for_notice(&http, &url, Duration::from_secs(5)).await;
            tracing::warn!(notice = %body, "spot interruption notice");
            let _ = out.send(AgentMsg::Draining {
                reason: format!("spot interruption notice: {}", body.trim()),
            });
        })
    });

    let cfg = Arc::new(agent.cfg);
    let (scene_tx, scene_rx) = watch::channel::<Option<SharedScene>>(None);
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
            CoordMsg::Assign { slots: list } => {
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
                );
                tracing::debug!(slots = list.len(), "assignment applied");
            }
        }
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
    ports: &HashMap<String, u16>,
    gpu_locks: &HashMap<String, Arc<Mutex<()>>>,
    cfg: &Arc<AgentCfg>,
    out: &mpsc::UnboundedSender<AgentMsg>,
    http: &reqwest::Client,
    scene_rx: &watch::Receiver<Option<SharedScene>>,
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
        };
        if let Some(h) = slots.get(&a.id) {
            h.desired.send_if_modified(|d| {
                // Generations only grow; an older assignment never moves a slot back.
                let next = Desired {
                    generation: d.generation.max(want.generation),
                    rect: want.rect,
                };
                let changed = *d != next;
                *d = next;
                changed
            });
            continue;
        }
        let (Some(&port), Some(lock)) = (ports.get(&a.sbdf), gpu_locks.get(&a.sbdf)) else {
            tracing::warn!(slot = %a.id, sbdf = %a.sbdf, "assigned a GPU this host does not have");
            let _ = out.send(AgentMsg::Slot(fleet_core::protocol::SlotReport {
                id: a.id.clone(),
                phase: "failed".into(),
                generation: a.generation,
                vm: None,
                frame_url: None,
                timings: Default::default(),
                error: Some(format!("GPU {} is not on this host", a.sbdf)),
            }));
            continue;
        };
        let (tx, rx) = watch::channel(want);
        let ctx = SlotCtx {
            id: a.id.clone(),
            sbdf: a.sbdf.clone(),
            port,
            cfg: cfg.clone(),
            out: out.clone(),
            gpu_lock: lock.clone(),
            http: http.clone(),
        };
        let task = tokio::spawn(supervise(ctx, rx, scene_rx.clone()));
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
