//! One instance of a piece: prepare (untimed), run the piece (timed), hold, tear down (timed).

use anyhow::{bail, Context, Result};
use fcvm::network::{NetworkManager, PastaNetwork};
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::time::Instant;
use tokio::io::AsyncBufReadExt;

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Piece {
    /// Nothing: the cost of an instance process itself (exec, runtime, pipes).
    Noop,
    /// The clone's state: claim its name, allocate its loopback IP, save its state.
    State,
    /// The clone's disk directory from the snapshot's (`DiskManager::create_cow_disk`).
    Disk,
    /// The rootless namespace holder (`spawn_namespace_holder`: unshare, uid/gid maps).
    Holder,
    /// The rootless network: holder, TAP setup script, pasta and the bridge.
    RootlessNet,
    /// A Firecracker process to its API socket, inside a prepared holder's namespaces.
    FcSpawn,
    /// Firecracker spawn, snapshot load and resume, on a prepared namespace and disk.
    Vmm,
    /// One HTTP health probe into a prepared namespace.
    Health,
    /// A bare KVM VM as Firecracker builds one on arm64 (VM, 256 MiB memory slot, one vCPU,
    /// a GICv3), no VMM process: the floor under `fc-spawn` and `vmm`.
    KvmVm,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prober {
    /// fcvm's probe: `nsenter -U -n -- curl` (`health::check_http_health_nsenter`).
    Nsenter,
}

#[derive(clap::Args, Clone, Debug)]
pub struct PieceArgs {
    #[arg(long, value_enum)]
    pub piece: Piece,
    /// Where instances keep their state files and data directories.
    #[arg(long, default_value = "/mnt/fcvm-btrfs/pieces")]
    pub work_dir: PathBuf,
    /// A snapshot directory (`snapshots/<tag>`) for the disk and vmm pieces.
    #[arg(long)]
    pub snapshot_dir: Option<PathBuf>,
    /// The Firecracker binary for the fc-spawn and vmm pieces.
    #[arg(long)]
    pub firecracker: Option<PathBuf>,
    /// A `snapshot serve` socket: the vmm piece loads through UFFD instead of the File backend.
    #[arg(long)]
    pub uffd_socket: Option<PathBuf>,
    /// The disk piece reflinks from one of the files in this directory (chosen by PID)
    /// instead of the snapshot's disk: an experiment on whether the shared source serializes.
    #[arg(long)]
    pub disk_shard_dir: Option<PathBuf>,
    /// Dirty page tracking on the load (fcvm clones default to on).
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub track_dirty_pages: bool,
    #[arg(long, value_enum, default_value = "nsenter")]
    pub prober: Prober,
}

/// What an instance made, held until teardown.
#[derive(Default)]
struct Held {
    vm_id: String,
    holder: Option<(tokio::process::Child, u32)>,
    network: Option<PastaNetwork>,
    disk: Option<fcvm::storage::DiskManager>,
    disk_path: Option<PathBuf>,
    vmm: Option<fcvm::firecracker::VmManager>,
    state: Option<fcvm::state::StateManager>,
    server: Option<std::process::Child>,
    runtime_dir: Option<PathBuf>,
    kvm: Option<crate::kvm::BareVm>,
}

/// The driver's PID, so an instance can tell it was reparented before arming its
/// parent-death signal.
pub const DRIVER_PID_ENV: &str = "START_PIECES_DRIVER_PID";

pub type Stages = Vec<(&'static str, f64)>;

pub fn ms(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1000.0
}

fn say(line: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

fn stages_json(op_ms: f64, stages: &Stages) -> String {
    let mut map = serde_json::Map::new();
    map.insert("ms".into(), op_ms.into());
    let s: serde_json::Map<String, serde_json::Value> = stages
        .iter()
        .map(|(k, v)| (k.to_string(), (*v).into()))
        .collect();
    map.insert("stages".into(), s.into());
    serde_json::Value::Object(map).to_string()
}

pub async fn instance(args: PieceArgs) -> Result<()> {
    // Die with the driver. Armed here rather than in a pre_exec (see `drive::spawn`); a
    // driver that died before this line has already reparented us, so check.
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } != 0 {
        bail!(
            "arming the parent-death signal: {}",
            std::io::Error::last_os_error()
        );
    }
    let driver = std::env::var(DRIVER_PID_ENV)
        .ok()
        .and_then(|p| p.parse::<u32>().ok());
    if driver != Some(std::os::unix::process::parent_id()) {
        bail!("the driver ({driver:?}) is not this instance's parent any more");
    }
    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    let mut held = Held {
        vm_id: fcvm::state::generate_vm_id(),
        ..Default::default()
    };
    if let Err(error) = prepare(&args, &mut held).await {
        say(&format!("error prepare: {error:#}"));
        let _ = teardown(&mut held).await;
        return Err(error);
    }
    say("ready");
    match lines.next_line().await? {
        Some(line) if line == "go" => {}
        _ => {
            teardown(&mut held).await?;
            return Ok(());
        }
    }
    let started = Instant::now();
    match op(&args, &mut held).await {
        Ok(stages) => say(&format!("done {}", stages_json(ms(started), &stages))),
        Err(error) => {
            say(&format!("error op: {error:#}"));
            let _ = teardown(&mut held).await;
            return Err(error);
        }
    }
    // Hold until told to go, or until the driver goes away.
    while let Some(line) = lines.next_line().await? {
        if line == "exit" {
            break;
        }
    }
    let started = Instant::now();
    let stages = teardown(&mut held).await?;
    say(&format!("torn {}", stages_json(ms(started), &stages)));
    Ok(())
}

fn tap_device(vm_id: &str) -> String {
    format!("tap-{}", fcvm::state::truncate_id(vm_id, 8))
}

fn pasta_network(vm_id: &str) -> PastaNetwork {
    PastaNetwork::new(vm_id.to_string(), tap_device(vm_id), Vec::new()).with_restore_mode()
}

async fn spawn_holder(held: &mut Held) -> Result<u32> {
    let cmd = pasta_network(&held.vm_id).build_holder_command();
    let (child, pid) = fcvm::commands::common::spawn_namespace_holder(&cmd).await?;
    held.holder = Some((child, pid));
    Ok(pid)
}

/// The rootless network the way `prepare_clone_substrate` and `restore_from_snapshot` make it.
async fn rootless_network(held: &mut Held, stages: &mut Stages) -> Result<u32> {
    // The clone's runtime directory, where pasta writes its PID file; fcvm makes it first.
    runtime_dir(held).await?;
    let mut network = pasta_network(&held.vm_id);
    let t = Instant::now();
    network.setup().await.context("network setup")?;
    stages.push(("config", ms(t)));
    let t = Instant::now();
    let pid = spawn_holder(held).await?;
    stages.push(("holder", ms(t)));
    let t = Instant::now();
    fcvm::commands::common::run_namespace_setup_script(
        pid,
        &network.build_nsenter_prefix(pid),
        &network.build_setup_script(),
    )
    .await?;
    stages.push(("tap", ms(t)));
    let t = Instant::now();
    let result = network.post_start(pid).await;
    held.network = Some(network);
    result.context("pasta post_start")?;
    stages.push(("pasta+bridge", ms(t)));
    Ok(pid)
}

/// The clone's runtime directory (`vm-disks/<vm_id>`), as `cmd_snapshot_run_inner` makes it.
async fn runtime_dir(held: &mut Held) -> Result<PathBuf> {
    let dir = fcvm::paths::vm_runtime_dir(&held.vm_id);
    tokio::fs::create_dir_all(&dir).await?;
    held.runtime_dir = Some(dir.clone());
    Ok(dir)
}

fn snapshot_dir(args: &PieceArgs) -> Result<&PathBuf> {
    args.snapshot_dir
        .as_ref()
        .context("this piece needs --snapshot-dir")
}

async fn clone_disk(args: &PieceArgs, held: &mut Held) -> Result<PathBuf> {
    let base = match &args.disk_shard_dir {
        Some(dir) => {
            let mut shards: Vec<PathBuf> = std::fs::read_dir(dir)?
                .map(|e| e.map(|e| e.path()))
                .collect::<std::io::Result<_>>()?;
            shards.sort();
            anyhow::ensure!(!shards.is_empty(), "{} holds no disk", dir.display());
            shards[std::process::id() as usize % shards.len()].clone()
        }
        None => snapshot_dir(args)?.join("disk").join("disk.raw"),
    };
    let vm_dir = runtime_dir(held).await?.join("disks");
    let disk = fcvm::storage::DiskManager::new(held.vm_id.clone(), base, vm_dir);
    let result = disk.create_cow_disk().await;
    held.disk = Some(disk);
    let path = result?;
    held.disk_path = Some(path.clone());
    Ok(path)
}

/// An address and responder inside the holder's network namespace, standing in for the guest.
const PROBE_IP: &str = "10.0.2.100";
const PROBE_PORT: u16 = 80;

async fn prepare(args: &PieceArgs, held: &mut Held) -> Result<()> {
    match args.piece {
        Piece::Noop
        | Piece::State
        | Piece::Disk
        | Piece::Holder
        | Piece::RootlessNet
        | Piece::KvmVm => {}
        Piece::FcSpawn => {
            spawn_holder(held).await?;
        }
        Piece::Vmm => {
            snapshot_dir(args)?;
            rootless_network(held, &mut Vec::new()).await?;
            clone_disk(args, held).await?;
        }
        Piece::Health => {
            let pid = spawn_holder(held).await?;
            let prefix = pasta_network(&held.vm_id).build_nsenter_prefix(pid);
            let status = tokio::process::Command::new(&prefix[0])
                .args(&prefix[1..])
                .args(["sh", "-c"])
                .arg(format!(
                    "ip link set lo up && ip addr add {PROBE_IP}/32 dev lo"
                ))
                .status()
                .await?;
            if !status.success() {
                bail!("configuring the probe address in the namespace: {status}");
            }
            let exe = std::env::current_exe()?;
            let mut cmd = std::process::Command::new(&prefix[0]);
            cmd.args(&prefix[1..])
                .arg(exe)
                .arg("http-serve")
                .arg(format!("{PROBE_IP}:{PROBE_PORT}"));
            unsafe {
                cmd.pre_exec(|| {
                    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            held.server = Some(cmd.spawn().context("starting the probe responder")?);
            let deadline = Instant::now() + std::time::Duration::from_secs(10);
            loop {
                if probe(args, pid).await.unwrap_or(false) {
                    break;
                }
                if Instant::now() > deadline {
                    bail!("the probe responder did not answer within 10 s");
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        }
    }
    Ok(())
}

async fn probe(args: &PieceArgs, holder_pid: u32) -> Result<bool> {
    match args.prober {
        Prober::Nsenter => {
            fcvm::health::check_http_health_nsenter(holder_pid, PROBE_IP, PROBE_PORT, "/", None, 5)
                .await
        }
    }
}

async fn op(args: &PieceArgs, held: &mut Held) -> Result<Stages> {
    let mut stages = Stages::new();
    match args.piece {
        Piece::Noop => {}
        Piece::State => {
            let dir = args.work_dir.join("state");
            let manager = fcvm::state::StateManager::new(dir);
            manager.init().await?;
            let mut vm_state =
                fcvm::state::VmState::new(held.vm_id.clone(), "start-pieces".into(), 1, 256);
            vm_state.name = Some(held.vm_id.clone());
            let t = Instant::now();
            manager.claim_name(&mut vm_state).await?;
            stages.push(("claim", ms(t)));
            let t = Instant::now();
            manager.allocate_loopback_ip(&mut vm_state).await?;
            stages.push(("loopback", ms(t)));
            let t = Instant::now();
            manager
                .update_state(&held.vm_id, |s| s.holder_pid = Some(std::process::id()))
                .await?;
            stages.push(("update", ms(t)));
            held.state = Some(manager);
        }
        Piece::Disk => {
            clone_disk(args, held).await?;
        }
        Piece::Holder => {
            spawn_holder(held).await?;
        }
        Piece::RootlessNet => {
            rootless_network(held, &mut stages).await?;
        }
        Piece::FcSpawn => {
            let pid = held.holder.as_ref().map(|(_, p)| *p).context("no holder")?;
            let firecracker = args.firecracker.as_ref().context("needs --firecracker")?;
            let dir = runtime_dir(held).await?;
            let mut vmm = fcvm::firecracker::VmManager::new(
                held.vm_id.clone(),
                dir.join("firecracker.sock"),
                None,
            );
            vmm.set_user_namespace_path(PathBuf::from(format!("/proc/{pid}/ns/user")));
            vmm.set_net_namespace_path(PathBuf::from(format!("/proc/{pid}/ns/net")));
            let result = vmm.start(firecracker, None, None).await;
            held.vmm = Some(vmm);
            result?;
        }
        Piece::Vmm => {
            restore(args, held, &mut stages).await?;
        }
        Piece::KvmVm => {
            held.kvm = Some(crate::kvm::BareVm::create(&mut stages)?);
        }
        Piece::Health => {
            let pid = held.holder.as_ref().map(|(_, p)| *p).context("no holder")?;
            if !probe(args, pid).await? {
                bail!("probe failed");
            }
        }
    }
    Ok(stages)
}

/// Firecracker spawn, snapshot load with every path overridden, resume: what
/// `restore_from_snapshot` does between its substrate and its state save.
async fn restore(args: &PieceArgs, held: &mut Held, stages: &mut Stages) -> Result<()> {
    use fcvm::firecracker::api::{
        DriveOverride, MemBackend, NetworkOverride, SnapshotLoad, VmState as ApiVmState,
        VsockOverride,
    };
    let snapshot = snapshot_dir(args)?;
    let pid = held.holder.as_ref().map(|(_, p)| *p).context("no holder")?;
    let firecracker = args.firecracker.as_ref().context("needs --firecracker")?;
    let disk = held.disk_path.clone().context("no disk")?;
    let dir = fcvm::paths::vm_runtime_dir(&held.vm_id);
    let t = Instant::now();
    let mut vmm =
        fcvm::firecracker::VmManager::new(held.vm_id.clone(), dir.join("firecracker.sock"), None);
    vmm.set_user_namespace_path(PathBuf::from(format!("/proc/{pid}/ns/user")));
    vmm.set_net_namespace_path(PathBuf::from(format!("/proc/{pid}/ns/net")));
    let started = vmm.start(firecracker, None, None).await;
    held.vmm = Some(vmm);
    started?;
    stages.push(("spawn", ms(t)));
    let client = held.vmm.as_ref().unwrap().client()?;
    let mem_backend = match &args.uffd_socket {
        Some(socket) => MemBackend {
            backend_type: "Uffd".into(),
            backend_path: socket.display().to_string(),
        },
        None => MemBackend {
            backend_type: "File".into(),
            backend_path: snapshot.join("memory.bin").display().to_string(),
        },
    };
    let t = Instant::now();
    client
        .load_snapshot(SnapshotLoad {
            snapshot_path: snapshot.join("vmstate.bin").display().to_string(),
            mem_backend,
            track_dirty_pages: Some(args.track_dirty_pages),
            resume_vm: Some(false),
            network_overrides: Some(vec![NetworkOverride {
                iface_id: "eth0".into(),
                host_dev_name: tap_device(&held.vm_id),
            }]),
            vsock_override: Some(VsockOverride {
                uds_path: dir.join("vsock.sock").display().to_string(),
            }),
            drive_overrides: Some(vec![DriveOverride {
                drive_id: "rootfs".into(),
                path_on_host: disk.display().to_string(),
            }]),
            device_passthrough: None,
            huge_pages: None,
        })
        .await
        .context("loading snapshot")?;
    stages.push(("load", ms(t)));
    let t = Instant::now();
    client
        .patch_vm_state(ApiVmState {
            state: "Resumed".into(),
        })
        .await
        .context("resuming")?;
    stages.push(("resume", ms(t)));
    Ok(())
}

async fn teardown(held: &mut Held) -> Result<Stages> {
    let mut stages = Stages::new();
    if let Some(vm) = held.kvm.take() {
        let t = Instant::now();
        drop(vm);
        stages.push(("kvm", ms(t)));
    }
    if let Some(mut vmm) = held.vmm.take() {
        let t = Instant::now();
        let _ = vmm.kill().await;
        stages.push(("vmm", ms(t)));
    }
    if let Some(mut server) = held.server.take() {
        let _ = server.kill();
        let _ = server.wait();
    }
    if let Some(mut network) = held.network.take() {
        let t = Instant::now();
        network.cleanup().await?;
        stages.push(("pasta", ms(t)));
    }
    if let Some((mut child, _)) = held.holder.take() {
        let t = Instant::now();
        let _ = child.kill().await;
        stages.push(("holder", ms(t)));
    }
    if let Some(disk) = held.disk.take() {
        let t = Instant::now();
        disk.cleanup().await?;
        stages.push(("disk", ms(t)));
    }
    if let Some(dir) = held.runtime_dir.take() {
        let t = Instant::now();
        match tokio::fs::remove_dir_all(&dir).await {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
        stages.push(("runtime-dir", ms(t)));
    }
    if let Some(manager) = held.state.take() {
        let t = Instant::now();
        manager.delete_state(&held.vm_id).await?;
        stages.push(("state", ms(t)));
    }
    Ok(stages)
}

/// Answer every connection with an empty 200 and close it.
pub fn http_serve(addr: &str) -> Result<()> {
    use std::io::Read;
    let listener = std::net::TcpListener::bind(addr).with_context(|| format!("binding {addr}"))?;
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let mut buf = [0u8; 1024];
        let _ = stream.read(&mut buf);
        let _ = stream.write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 0\r\n\r\n");
    }
    Ok(())
}
