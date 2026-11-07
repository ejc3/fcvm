//! `fcvm vm`: raw VMs, whose guest runs no fc-agent (Windows Server, booted from UEFI
//! firmware on Cloud Hypervisor). See docs/fleet/windows-sandboxes.md, "A raw VM in fcvm".
//!
//! fcvm boots, snapshots and restores a raw VM and does nothing inside it: no container, no
//! boot plan, no vsock, no exec, no volumes. Its namespace has no pasta and no route out
//! (`PastaNetwork::isolated`). One helper runs in that namespace ([`NsHelper`]): it relays
//! the VM's `rdp.sock` to the guest's Remote Desktop port, which is how the bridge and the
//! health probe reach it, and it serves the clock line on 10.0.2.1:4600, from which the
//! guest agent learns it was restored and the host's time, and to which it acknowledges.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::cli::{NetworkMode, NsHelperArgs, VmArgs, VmCommands, VmRunArgs};
use crate::commands::common::{LifecycleReadyGate, LifecycleReadyOutcome};
use crate::hypervisor::cloud_hypervisor::CloudHypervisorBackend;
use crate::hypervisor::{DriveSpec, Hypervisor, NetIfaceSpec, ProcessSpec};
use crate::network::{NetworkManager, PastaNetwork};
use crate::paths;
use crate::state::{generate_vm_id, truncate_id, validate_vm_name, StateManager, VmState};

/// Where a raw VM's clock line listens inside its namespace: the namespace bridge's
/// address, which the guest reaches as its on-link neighbour.
pub const CLOCK_LINE_ADDR: &str = "10.0.2.1:4600";

/// The guest's address inside its namespace, set statically by its agent.
pub const RAW_GUEST_IP: std::net::Ipv4Addr = std::net::Ipv4Addr::new(10, 0, 2, 100);

/// The Unix socket in a raw VM's data directory that reaches its Remote Desktop port.
pub const RDP_SOCKET: &str = "rdp.sock";

/// The file in a raw VM's data directory holding the guest agent's last acknowledgement:
/// `<generation> <guest unix ms> <host unix ms when it arrived>`.
pub const CLOCK_ACK_FILE: &str = "clock-ack";

/// The largest difference between the guest's clock in an acknowledgement and the host's
/// when it arrived that still counts as the guest's clock being set.
pub const CLOCK_ACK_MAX_SKEW_MS: u64 = 1000;

/// How long the clock line waits for the guest's acknowledgement on a connection.
const CLOCK_ACK_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// The longest acknowledgement line read (`ack <vm id> <ms>` is about 60 bytes).
const CLOCK_ACK_MAX_LINE: u64 = 256;

/// How long the RDP relay waits for the guest to accept a connection.
const RDP_RELAY_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// What the helper prints on stdout once both listeners are bound.
const NS_HELPER_READY: &str = "listening";

/// How long the helper has to bind and say so.
const NS_HELPER_START_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// A raw VM's `rdp.sock`.
pub fn rdp_socket_path(data_dir: &Path) -> PathBuf {
    data_dir.join(RDP_SOCKET)
}

/// A raw VM's acknowledgement file.
pub fn clock_ack_path(data_dir: &Path) -> PathBuf {
    data_dir.join(CLOCK_ACK_FILE)
}

pub async fn cmd_vm(args: VmArgs) -> Result<()> {
    match args.cmd {
        VmCommands::Run(args) => cmd_vm_run(args).await,
        VmCommands::NsHelper(args) => cmd_ns_helper(args).await,
    }
}

fn unix_ms(time: std::time::SystemTime) -> u64 {
    time.duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// The line the clock line serves: the VM's generation (its id, new with every restore) and
/// the host's wall clock in milliseconds since the Unix epoch.
pub fn clock_line(generation: &str, now: std::time::SystemTime) -> String {
    format!("{generation} {}\n", unix_ms(now))
}

/// The guest time in an acknowledgement of `generation` (`ack <generation> <guest unix ms>`),
/// or None for anything else, an acknowledgement of another generation included.
pub fn parse_clock_ack(line: &str, generation: &str) -> Option<u64> {
    let mut fields = line.trim_end_matches(['\r', '\n']).split(' ');
    match (fields.next(), fields.next(), fields.next(), fields.next()) {
        (Some("ack"), Some(acked), Some(ms), None) if acked == generation => ms.parse().ok(),
        _ => None,
    }
}

/// Write the acknowledgement file whole: to a temporary file beside it, then renamed over it,
/// so the health monitor never reads a partial record.
fn write_clock_ack(path: &Path, generation: &str, guest_ms: u64, host_ms: u64) -> Result<()> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("ack");
    let tmp = path.with_file_name(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::write(&tmp, format!("{generation} {guest_ms} {host_ms}\n"))
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| {
        let _ = std::fs::remove_file(&tmp);
        format!("renaming {} to {}", tmp.display(), path.display())
    })
}

/// Serve one clock line connection: write the line, then read one acknowledgement within
/// [`CLOCK_ACK_READ_TIMEOUT`] and record it when it names this generation.
async fn clock_line_connection(
    stream: tokio::net::TcpStream,
    generation: &str,
    ack_file: &Path,
) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
    let (reader, mut writer) = stream.into_split();
    writer
        .write_all(clock_line(generation, std::time::SystemTime::now()).as_bytes())
        .await
        .context("writing the clock line")?;
    let mut line = String::new();
    let mut reader = tokio::io::BufReader::new(reader.take(CLOCK_ACK_MAX_LINE));
    match tokio::time::timeout(CLOCK_ACK_READ_TIMEOUT, reader.read_line(&mut line)).await {
        Ok(Ok(_)) => {}
        // No acknowledgement on this connection: the agent sends one only for a generation
        // it has not seen.
        Ok(Err(_)) | Err(_) => return Ok(()),
    }
    let host_ms = unix_ms(std::time::SystemTime::now());
    if let Some(guest_ms) = parse_clock_ack(&line, generation) {
        write_clock_ack(ack_file, generation, guest_ms, host_ms)?;
    }
    Ok(())
}

/// Serve the clock line on `listener`, each connection on its own task. Runs until the
/// listener fails.
pub async fn serve_clock_line(
    listener: tokio::net::TcpListener,
    generation: String,
    ack_file: PathBuf,
) -> Result<()> {
    let generation = std::sync::Arc::<str>::from(generation);
    let ack_file = std::sync::Arc::<Path>::from(ack_file);
    loop {
        let (stream, peer) = listener.accept().await.context("accepting a clock line")?;
        let (generation, ack_file) = (generation.clone(), ack_file.clone());
        tokio::spawn(async move {
            if let Err(error) = clock_line_connection(stream, &generation, &ack_file).await {
                eprintln!("clock line: {peer}: {error:#}");
            }
        });
    }
}

/// Relay every connection to `listener` to `target`, both ways, each on its own task. Runs
/// until the listener fails.
pub async fn serve_rdp_relay(
    listener: tokio::net::UnixListener,
    target: std::net::SocketAddr,
) -> Result<()> {
    loop {
        let (mut client, _) = listener.accept().await.context("accepting on rdp.sock")?;
        tokio::spawn(async move {
            let guest = tokio::time::timeout(
                RDP_RELAY_CONNECT_TIMEOUT,
                tokio::net::TcpStream::connect(target),
            )
            .await;
            match guest {
                Ok(Ok(mut guest)) => {
                    let _ = tokio::io::copy_bidirectional(&mut client, &mut guest).await;
                }
                Ok(Err(error)) => eprintln!("rdp relay: connecting to {target}: {error}"),
                Err(_) => eprintln!("rdp relay: {target} did not accept within 5 s"),
            }
        });
    }
}

/// `fcvm vm ns-helper`: bind `rdp.sock` and the clock line, say so on stdout, and serve both.
async fn cmd_ns_helper(args: NsHelperArgs) -> Result<()> {
    use std::io::Write;
    match std::fs::remove_file(&args.rdp_socket) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("removing {}", args.rdp_socket.display())),
    }
    let rdp = tokio::net::UnixListener::bind(&args.rdp_socket)
        .with_context(|| format!("binding {}", args.rdp_socket.display()))?;
    let clock = tokio::net::TcpListener::bind(CLOCK_LINE_ADDR)
        .await
        .with_context(|| format!("binding the clock line on {CLOCK_LINE_ADDR}"))?;
    {
        let mut stdout = std::io::stdout().lock();
        writeln!(stdout, "{NS_HELPER_READY}")?;
        stdout.flush()?;
    }
    // Point stdout at /dev/null, so the parent's pipe reaches EOF and it holds no reader for
    // the life of this child. Not a bare close: an accepted socket would then take fd 1.
    let null = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/null")
        .context("opening /dev/null")?;
    // SAFETY: dup2 onto this process's own stdout, which nothing writes to from here on.
    if unsafe { libc::dup2(std::os::fd::AsRawFd::as_raw_fd(&null), libc::STDOUT_FILENO) } < 0 {
        return Err(std::io::Error::last_os_error()).context("redirecting stdout");
    }
    drop(null);
    let target = std::net::SocketAddr::from((RAW_GUEST_IP, args.rdp_port));
    tokio::try_join!(
        serve_rdp_relay(rdp, target),
        serve_clock_line(clock, args.generation, args.ack_file),
    )?;
    Ok(())
}

/// The namespace helper of one raw VM: a `fcvm vm ns-helper` child in the VM's user and
/// network namespaces. It dies with this process (`PR_SET_PDEATHSIG`, set after it enters the
/// namespaces) and is killed and reaped by [`NsHelper::stop`].
pub struct NsHelper {
    child: tokio::process::Child,
}

impl NsHelper {
    /// Start the helper for `generation` in the namespaces at `user_ns` and `net_ns`, relaying
    /// `<data_dir>/rdp.sock` to the guest's `rdp_port` and writing acknowledgements to
    /// `<data_dir>/clock-ack`, and return once both listeners are bound. The clock line's
    /// address is on the namespace bridge, so this runs after the network's `post_start`.
    pub async fn start(
        user_ns: &Path,
        net_ns: &Path,
        generation: &str,
        data_dir: &Path,
        rdp_port: u16,
    ) -> Result<NsHelper> {
        use tokio::io::AsyncBufReadExt;
        let exe = std::env::current_exe().context("finding this fcvm binary")?;
        let mut command = tokio::process::Command::new(exe);
        command
            .args([
                "vm",
                "ns-helper",
                "--generation",
                generation,
                "--rdp-socket",
            ])
            .arg(rdp_socket_path(data_dir))
            .args(["--rdp-port", &rdp_port.to_string(), "--ack-file"])
            .arg(clock_ack_path(data_dir))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true);
        // setns into the user namespace in pre_exec, then PR_SET_PDEATHSIG, as every VMM is
        // spawned: nsenter enters the user namespace after a pdeathsig set before its exec,
        // and the credential change can clear it (see install_namespace_pre_exec).
        crate::utils::install_namespace_pre_exec(
            &mut command,
            &crate::utils::NamespaceParams {
                vm_id: generation.to_string(),
                user_namespace_path: Some(user_ns.to_path_buf()),
                net_namespace_path: Some(net_ns.to_path_buf()),
                ..Default::default()
            },
        )?;
        // The parent died between the fork and the prctl above: no signal will come.
        // SAFETY: getpid() before the fork; only getppid and _exit run in the child.
        let parent = unsafe { libc::getpid() };
        unsafe {
            command.pre_exec(move || {
                if libc::getppid() != parent {
                    libc::_exit(1);
                }
                Ok(())
            });
        }
        let mut child = command.spawn().context("starting the namespace helper")?;
        let stdout = child
            .stdout
            .take()
            .context("the namespace helper's stdout")?;
        let mut first = String::new();
        let read = tokio::time::timeout(
            NS_HELPER_START_TIMEOUT,
            tokio::io::BufReader::new(stdout).read_line(&mut first),
        )
        .await;
        if !matches!(read, Ok(Ok(_)) if first.trim_end() == NS_HELPER_READY) {
            let _ = child.start_kill();
            let status = child.wait().await;
            bail!(
                "the namespace helper did not start (read {:?}, exit {:?})",
                read.map(|r| r.map(|_| first.trim_end().to_string())),
                status
            );
        }
        info!(
            generation,
            clock_line = CLOCK_LINE_ADDR,
            rdp_port,
            "namespace helper listening"
        );
        Ok(NsHelper { child })
    }

    /// Kill the helper and reap it.
    pub async fn stop(mut self) {
        let _ = self.child.start_kill();
        if let Err(error) = self.child.wait().await {
            warn!(%error, "reaping the namespace helper");
        }
    }
}

/// Stop a namespace helper a raw VM may or may not have started.
pub(crate) async fn stop_ns_helper(helper: &mut Option<NsHelper>) {
    if let Some(helper) = helper.take() {
        helper.stop().await;
    }
}

/// Refuse a `vm run` fcvm cannot boot, before anything is allocated.
fn check_vm_run_args(args: &VmRunArgs, host_arch: &str) -> Result<()> {
    anyhow::ensure!(
        host_arch == "x86_64",
        "Windows guests need an x86_64 host (this host is {host_arch})"
    );
    anyhow::ensure!(
        args.network == NetworkMode::Rootless,
        "a raw VM supports only --network rootless, a namespace with no route out (got {:?})",
        args.network
    );
    anyhow::ensure!(
        args.firmware.is_file(),
        "--firmware {}: no such file",
        args.firmware.display()
    );
    anyhow::ensure!(
        args.disk.is_file(),
        "--disk {}: no such file",
        args.disk.display()
    );
    if let Some(target) = args.balloon {
        anyhow::ensure!(
            target <= args.mem,
            "--balloon {target} is above --mem {}: a balloon target cannot exceed the guest's memory",
            args.mem
        );
    }
    Ok(())
}

/// `fcvm vm run`: boot a raw VM, supervise it until its VMM exits or a signal arrives, then
/// tear it down.
async fn cmd_vm_run(args: VmRunArgs) -> Result<()> {
    check_vm_run_args(&args, std::env::consts::ARCH)?;
    validate_vm_name(&args.name).context("invalid VM name")?;
    let ch_bin = crate::commands::common::find_cloud_hypervisor()?;

    let lifecycle_gate = LifecycleReadyGate::new();
    crate::commands::podman::spawn_lifecycle_signal_handler("shutting down VM")?;

    // What VMs of processes killed without teardown left goes first, then this VM's
    // runtime directory is recorded before anything creates it (crate::reclaim).
    crate::reclaim::reclaim_abandoned().await;
    let vm_id = generate_vm_id();
    let reclaim_owner = crate::reclaim::Owner::begin(&vm_id)?;
    reclaim_owner.record(crate::reclaim::Resource::RuntimeDir {
        path: paths::vm_runtime_dir(&vm_id),
    })?;
    paths::check_socket_path_budget(&vm_id)?;
    let data_dir = paths::vm_runtime_dir(&vm_id);
    tokio::fs::create_dir_all(&data_dir)
        .await
        .context("creating VM data directory")?;

    let mut vm_state = VmState::new(
        vm_id.clone(),
        args.disk.display().to_string(),
        args.cpu,
        args.mem,
    );
    vm_state.name = Some(args.name.clone());
    vm_state.config.guest = args.guest.into();
    vm_state.config.firmware = Some(args.firmware.clone());
    vm_state.config.health_port = Some(args.health_port);
    vm_state.config.rdp_socket = Some(rdp_socket_path(&data_dir));
    vm_state.config.hypervisor = crate::hypervisor::Backend::CloudHypervisor;
    vm_state.config.network_mode = crate::firecracker::FcNetworkMode::Rootless;
    // Nothing is published: Remote Desktop is reached through the VM's rdp.sock.
    vm_state.config.published_guest_ports = Some(Vec::new());
    vm_state.config.balloon_mib = args.balloon;
    // No vsock device: nothing in the guest listens on one. The restore machinery still
    // keys the source VM's directory off the path its vsock socket would have had, which is
    // the directory that holds the disk and the console log a restore redirects.
    vm_state.config.vsock_socket_path = None;
    vm_state.config.source_vsock_socket_path = Some(data_dir.join("vsock.sock"));

    let state_manager = StateManager::new(paths::state_dir());
    state_manager.init().await?;
    if let Err(e) = state_manager
        .claim_name(&mut vm_state)
        .await
        .context("claiming the VM name")
    {
        crate::commands::podman::cleanup_failed_prepare(&state_manager, &vm_id, &data_dir).await;
        return Err(e);
    }

    let mut network =
        PastaNetwork::isolated(vm_id.clone(), format!("tap-{}", truncate_id(&vm_id, 8)));
    let network_config = match network.setup().await.context("setting up network") {
        Ok(config) => config,
        Err(e) => {
            if let Err(error) = network.cleanup().await {
                warn!(%error, "failed to clean up the network after a setup error");
            }
            crate::commands::podman::cleanup_failed_prepare(&state_manager, &vm_id, &data_dir)
                .await;
            return Err(e);
        }
    };

    // The boot publishes each process into its slot as soon as it exists, so a failure
    // partway kills what it started.
    let mut vm_slot: Option<Box<dyn Hypervisor>> = None;
    let mut holder_slot: Option<tokio::process::Child> = None;
    let mut helper_slot: Option<NsHelper> = None;
    let booted = boot_raw_vm(
        &args,
        &ch_bin,
        &data_dir,
        &mut network,
        &network_config,
        &mut vm_state,
        &state_manager,
        &mut vm_slot,
        &mut holder_slot,
        &mut helper_slot,
    )
    .await;
    if let Err(e) = booted {
        warn!(vm_id = %vm_id, error = %format!("{e:#}"), "raw VM boot failed, cleaning up");
        if let Some(vm) = vm_slot.as_mut() {
            if let Err(error) = vm.kill().await {
                warn!(%error, "failed to kill the VMM");
            }
        }
        if let Some(holder) = holder_slot.as_mut() {
            let _ = holder.kill().await;
        }
        stop_ns_helper(&mut helper_slot).await;
        if let Err(error) = network.cleanup().await {
            warn!(%error, "failed to clean up the network after a boot error");
        }
        crate::commands::podman::cleanup_failed_prepare(&state_manager, &vm_id, &data_dir).await;
        return Err(e);
    }
    info!(vm_id = %vm_id, name = %args.name, "raw VM booted");

    run_until_exit(
        RawVm {
            vm_id,
            data_dir,
            vm_manager: vm_slot.expect("boot_raw_vm sets the VMM on success"),
            holder_child: holder_slot,
            network: Box::new(network),
            ns_helper: helper_slot,
            state_manager,
            vm_state,
            reclaim_owner,
        },
        &lifecycle_gate,
    )
    .await
}

/// Cold-boot a raw VM: its reflink disk, the namespace (no route out), Cloud Hypervisor, the
/// bridge, the namespace helper, then the firmware boot. Nothing here talks to the
/// guest.
#[allow(clippy::too_many_arguments)]
async fn boot_raw_vm(
    args: &VmRunArgs,
    ch_bin: &Path,
    data_dir: &Path,
    network: &mut PastaNetwork,
    network_config: &crate::network::NetworkConfig,
    vm_state: &mut VmState,
    state_manager: &StateManager,
    vm_slot: &mut Option<Box<dyn Hypervisor>>,
    holder_slot: &mut Option<tokio::process::Child>,
    helper_slot: &mut Option<NsHelper>,
) -> Result<()> {
    let vm_id = vm_state.vm_id.clone();
    // The disk is the VM's own reflink copy, at the path every VM's disk has, which is
    // where `snapshot create` copies it from.
    let disk =
        crate::storage::DiskManager::new(vm_id.clone(), args.disk.clone(), data_dir.join("disks"))
            .create_cow_disk()
            .await
            .context("creating the VM's copy of the disk")?;

    let mut process_spec = ProcessSpec {
        binary: ch_bin.to_path_buf(),
        vm_name: Some(args.name.clone()),
        ..Default::default()
    };
    *holder_slot = Some(
        crate::commands::podman::setup_rootless_namespace(
            network,
            network_config,
            &mut process_spec,
            vm_state,
        )
        .await?,
    );
    let holder_pid = vm_state
        .holder_pid
        .context("the rootless namespace has no holder")?;

    let backend = vm_slot.insert(Box::new(CloudHypervisorBackend::new(
        vm_id.clone(),
        data_dir.join("firecracker.sock"),
        Some(data_dir.join("firecracker.log")),
    )) as Box<dyn Hypervisor>);
    backend
        .spawn(&process_spec)
        .await
        .context("starting Cloud Hypervisor")?;

    network
        .post_start(holder_pid)
        .await
        .context("post-start network setup")?;

    let (user_ns, net_ns) = process_spec
        .user_namespace_path
        .as_deref()
        .zip(process_spec.net_namespace_path.as_deref())
        .context("the rootless namespace has no namespace paths")?;
    *helper_slot =
        Some(NsHelper::start(user_ns, net_ns, &vm_id, data_dir, args.health_port).await?);

    backend
        .as_any_mut()
        .downcast_mut::<CloudHypervisorBackend>()
        .context("the raw VM's backend is Cloud Hypervisor")?
        .configure_firmware_boot(&args.firmware, args.cpu, args.mem);
    backend
        .add_drive(&DriveSpec {
            drive_id: "rootfs".to_string(),
            path_on_host: disk,
            is_root_device: true,
            is_read_only: false,
        })
        .await?;
    backend
        .add_network_interface(&NetIfaceSpec {
            iface_id: "eth0".to_string(),
            host_dev_name: network_config.tap_device.clone(),
            guest_mac: Some(network_config.guest_mac.clone()),
        })
        .await?;
    backend.add_entropy_device().await?;
    if let Some(target_mib) = args.balloon {
        backend
            .add_balloon(crate::firecracker::BalloonDevice {
                target_mib,
                free_page_reporting: false,
            })
            .await?;
    }
    backend.boot().await.context("booting the firmware")?;

    crate::commands::common::save_vm_state_with_network(state_manager, vm_state, network_config)
        .await
}

/// What a running raw VM owns, cold-booted or restored.
pub(crate) struct RawVm {
    pub vm_id: String,
    pub data_dir: PathBuf,
    pub vm_manager: Box<dyn Hypervisor>,
    pub holder_child: Option<tokio::process::Child>,
    pub network: Box<dyn NetworkManager>,
    pub ns_helper: Option<NsHelper>,
    pub state_manager: StateManager,
    pub vm_state: VmState,
    pub reclaim_owner: crate::reclaim::Owner,
}

/// Publish a raw VM's lifecycle, watch its health (the clock acknowledgement and an RDP
/// probe through rdp.sock) and run it until its VMM exits or this process is asked to stop,
/// then tear everything down: the namespace helper, the VMM, the holder, the network, the
/// state and the data directory.
pub(crate) async fn run_until_exit(
    mut vm: RawVm,
    lifecycle_gate: &LifecycleReadyGate,
) -> Result<()> {
    let cancel = lifecycle_gate.cancellation_token();
    let health_cancel = CancellationToken::new();
    let health = crate::health::spawn_health_monitor_full(
        vm.vm_id.clone(),
        vm.vm_state.pid,
        paths::state_dir(),
        Some(health_cancel.clone()),
        None,
        None,
    );

    let result = if cancel.is_cancelled() {
        Err(anyhow::anyhow!("interrupted by signal during VM setup"))
    } else {
        match lifecycle_gate
            .publish(&vm.state_manager, &mut vm.vm_state)
            .await
        {
            Ok(LifecycleReadyOutcome::Published) => {
                tokio::select! {
                    status = vm.vm_manager.wait() => match status {
                        Ok(status) if status.success() => {
                            info!(vm_id = %vm.vm_id, "the raw VM's VMM exited");
                            Ok(())
                        }
                        Ok(status) => Err(anyhow::anyhow!(
                            "Cloud Hypervisor exited with {status}"
                        )),
                        Err(e) => Err(e.context("waiting for Cloud Hypervisor")),
                    },
                    _ = cancel.cancelled() => {
                        info!(vm_id = %vm.vm_id, "shutdown requested, stopping the raw VM");
                        Ok(())
                    }
                }
            }
            Ok(LifecycleReadyOutcome::Cancelled) => {
                Err(anyhow::anyhow!("interrupted by signal during VM setup"))
            }
            Err(e) => Err(e),
        }
    };

    stop_ns_helper(&mut vm.ns_helper).await;
    crate::commands::common::cleanup_vm(
        crate::commands::common::CleanupContext {
            vm_id: vm.vm_id,
            volume_server_handles: Vec::new(),
            remap_refs: Vec::new(),
            data_dir: vm.data_dir,
            health_cancel_token: Some(health_cancel),
            health_monitor_handle: Some(health),
            output_listener_handle: None,
            container_log: None,
        },
        vm.vm_manager.as_mut(),
        &mut vm.holder_child,
        vm.network.as_mut(),
        &vm.state_manager,
    )
    .await;
    drop(vm.reclaim_owner);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse(args: &[&str]) -> Result<crate::cli::Cli, clap::Error> {
        crate::cli::Cli::try_parse_from(std::iter::once("fcvm").chain(args.iter().copied()))
    }

    fn run_args(args: &[&str]) -> VmRunArgs {
        match parse(args).unwrap().cmd {
            crate::cli::Commands::Vm(VmArgs {
                cmd: VmCommands::Run(run),
            }) => run,
            other => panic!("not vm run: {other:?}"),
        }
    }

    /// `vm run` takes a name, firmware, disk and guest, and defaults to 2 vCPUs, 4 GiB,
    /// RDP's port, the raw network and no balloon. Without --firmware, --disk or --guest it
    /// does not parse, --guest names Windows only, and there is no --publish.
    #[test]
    fn vm_run_parses_its_arguments() {
        let base = [
            "vm",
            "run",
            "--name",
            "w0",
            "--firmware",
            "/fw/CLOUDHV.fd",
            "--disk",
            "/img/win.raw",
            "--guest",
            "windows",
        ];
        let run = run_args(&base);
        assert_eq!(run.name, "w0");
        assert_eq!(run.firmware, PathBuf::from("/fw/CLOUDHV.fd"));
        assert_eq!(run.disk, PathBuf::from("/img/win.raw"));
        assert_eq!(run.cpu, 2);
        assert_eq!(run.mem, 4096);
        assert_eq!(run.guest, crate::cli::RawGuest::Windows);
        assert_eq!(run.health_port, 3389);
        assert_eq!(run.network, NetworkMode::Rootless);
        assert_eq!(run.balloon, None);

        let mut more = base.to_vec();
        more.extend([
            "--cpu",
            "4",
            "--mem",
            "8192",
            "--health-port",
            "3390",
            "--balloon",
            "1024",
        ]);
        let run = run_args(&more);
        assert_eq!((run.cpu, run.mem, run.health_port), (4, 8192, 3390));
        assert_eq!(run.balloon, Some(1024));

        for missing in ["--firmware", "--disk", "--guest"] {
            let at = base.iter().position(|a| *a == missing).unwrap();
            let mut args = base.to_vec();
            args.drain(at..at + 2);
            assert!(parse(&args).is_err(), "parsed without {missing}");
        }
        let mut linux = base.to_vec();
        *linux.last_mut().unwrap() = "linux";
        assert!(parse(&linux).is_err(), "--guest linux parsed");
        let mut publish = base.to_vec();
        publish.extend(["--publish", "13389:3389"]);
        assert!(parse(&publish).is_err(), "--publish parsed");
    }

    /// A `vm run` fcvm cannot boot is refused before anything is allocated: an aarch64
    /// host, a network other than the raw default, a missing firmware or disk, a balloon
    /// above the memory.
    #[test]
    fn vm_run_refuses_what_it_cannot_boot() {
        let dir = tempfile::tempdir().unwrap();
        let firmware = dir.path().join("CLOUDHV.fd");
        let disk = dir.path().join("win.raw");
        std::fs::write(&firmware, b"fw").unwrap();
        std::fs::write(&disk, b"disk").unwrap();
        let args = |extra: &[&str]| {
            let mut all = vec![
                "vm",
                "run",
                "--name",
                "w0",
                "--firmware",
                firmware.to_str().unwrap(),
                "--disk",
                disk.to_str().unwrap(),
                "--guest",
                "windows",
            ];
            all.extend_from_slice(extra);
            run_args(&all)
        };
        let refused = |args: &VmRunArgs, arch: &str| {
            format!("{:#}", check_vm_run_args(args, arch).unwrap_err())
        };

        check_vm_run_args(&args(&[]), "x86_64").expect("a bootable VM");
        assert!(refused(&args(&[]), "aarch64").contains("Windows guests need an x86_64 host"));
        for network in ["bridged", "routed"] {
            let error = refused(&args(&["--network", network]), "x86_64");
            assert!(error.contains("only --network rootless"), "{error}");
        }
        assert!(refused(&args(&["--balloon", "8192"]), "x86_64").contains("--balloon 8192"));

        let mut missing = args(&[]);
        missing.firmware = dir.path().join("absent.fd");
        assert!(refused(&missing, "x86_64").contains("--firmware"));
        let mut missing = args(&[]);
        missing.disk = dir.path().join("absent.raw");
        assert!(refused(&missing, "x86_64").contains("--disk"));
    }

    /// Connect to the clock line at `addr`, read its line, and answer with `reply` (none
    /// when it is empty). Returns the line read.
    async fn clock_exchange(addr: std::net::SocketAddr, reply: &str) -> String {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut line = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio::io::BufReader::new(reader).read_line(&mut line),
        )
        .await
        .expect("the clock line writes a line")
        .unwrap();
        if !reply.is_empty() {
            writer.write_all(reply.as_bytes()).await.unwrap();
        }
        drop(writer);
        line
    }

    /// Wait until `path` holds something, for at most 5 s.
    async fn read_when_written(path: &Path) -> Option<String> {
        for _ in 0..100 {
            if let Ok(text) = std::fs::read_to_string(path) {
                return Some(text);
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        None
    }

    /// Every connection to the clock line reads `<generation> <host unix ms>`, stamped when
    /// it was accepted. An `ack` of that generation is recorded with the guest's time and
    /// the host's when it arrived; an `ack` of another generation, a malformed one or none
    /// records nothing.
    #[tokio::test]
    async fn the_clock_line_serves_the_generation_and_records_its_ack() {
        let dir = tempfile::tempdir().unwrap();
        let ack = clock_ack_path(dir.path());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(serve_clock_line(
            listener,
            "vm-abc123".to_string(),
            ack.clone(),
        ));

        for reply in [
            "",
            "ack vm-old 1700000000000\n",
            "ack vm-abc123\n",
            "hello\n",
        ] {
            let before = unix_ms(std::time::SystemTime::now());
            let line = clock_exchange(addr, reply).await;
            let after = unix_ms(std::time::SystemTime::now());
            let (generation, ms) = line
                .strip_suffix('\n')
                .and_then(|l| l.split_once(' '))
                .unwrap_or_else(|| panic!("not one line of two fields: {line:?}"));
            assert_eq!(generation, "vm-abc123");
            let ms: u64 = ms.parse().unwrap();
            assert!(before <= ms && ms <= after, "{before} <= {ms} <= {after}");
        }
        // Each of those connections has been read and closed; none wrote the file.
        tokio::time::sleep(CLOCK_ACK_READ_TIMEOUT + std::time::Duration::from_millis(200)).await;
        assert!(!ack.exists(), "an ack of another generation was recorded");

        let guest_ms = unix_ms(std::time::SystemTime::now());
        clock_exchange(addr, &format!("ack vm-abc123 {guest_ms}\r\n")).await;
        let recorded = read_when_written(&ack).await.expect("the ack is recorded");
        let fields: Vec<&str> = recorded.split_whitespace().collect();
        assert_eq!(fields[..2], ["vm-abc123", &guest_ms.to_string()]);
        let host_ms: u64 = fields[2].parse().unwrap();
        assert!(host_ms >= guest_ms);
        server.abort();

        assert_eq!(
            clock_line(
                "vm-1",
                std::time::UNIX_EPOCH + std::time::Duration::from_millis(1_700_000_000_123)
            ),
            "vm-1 1700000000123\n"
        );
        assert_eq!(parse_clock_ack("ack vm-1 5\n", "vm-1"), Some(5));
        assert_eq!(parse_clock_ack("ack vm-1 5 6\n", "vm-1"), None);
        assert_eq!(parse_clock_ack("ack vm-2 5\n", "vm-1"), None);
    }

    /// Every connection to rdp.sock is relayed to the guest's port, both ways.
    #[tokio::test]
    async fn rdp_sock_relays_to_the_guest_port() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let dir = tempfile::tempdir().unwrap();
        let guest = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = guest.local_addr().unwrap();
        let echo = tokio::spawn(async move {
            let (mut socket, _) = guest.accept().await.unwrap();
            let mut request = [0u8; 5];
            socket.read_exact(&mut request).await.unwrap();
            socket
                .write_all(&request.map(|b| b.to_ascii_uppercase()))
                .await
                .unwrap();
        });
        let socket = rdp_socket_path(dir.path());
        let relay = tokio::spawn(serve_rdp_relay(
            tokio::net::UnixListener::bind(&socket).unwrap(),
            target,
        ));
        let mut client = tokio::net::UnixStream::connect(&socket).await.unwrap();
        client.write_all(b"hello").await.unwrap();
        let mut reply = [0u8; 5];
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.read_exact(&mut reply),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(&reply, b"HELLO");
        echo.await.unwrap();
        relay.abort();
    }

    /// The helper child is this fcvm's `vm ns-helper`, whose arguments parse as the hidden
    /// subcommand, and runs in the VM's namespaces with its parent-death signal set after it
    /// entered them and the parent re-checked after that.
    #[test]
    fn the_namespace_helper_enters_the_namespaces_before_its_death_signal() {
        let cli = parse(&[
            "vm",
            "ns-helper",
            "--generation",
            "vm-1",
            "--rdp-socket",
            "/vm/rdp.sock",
            "--rdp-port",
            "3389",
            "--ack-file",
            "/vm/clock-ack",
        ])
        .unwrap();
        assert!(matches!(
            cli.cmd,
            crate::cli::Commands::Vm(VmArgs {
                cmd: VmCommands::NsHelper(_)
            })
        ));
        let source = include_str!("vm.rs");
        let start = source.find("    pub async fn start(").unwrap();
        let body = &source[start..start + source[start..].find("pub async fn stop(").unwrap()];
        let entered = body.find("install_namespace_pre_exec(").unwrap();
        let rechecked = body.find("libc::getppid()").unwrap();
        let spawned = body.find("command.spawn()").unwrap();
        assert!(entered < rechecked && rechecked < spawned);
    }

    /// A cold boot runs in an isolated namespace (no pasta, no route out), starts the
    /// namespace helper after the bridge is up (which adds 10.0.2.1) and before the
    /// firmware boots, and never configures a vsock device or a boot plan.
    #[test]
    fn a_raw_cold_boot_serves_the_helper_before_the_guest_runs() {
        let source = include_str!("vm.rs");
        let run = &source[source.find("async fn cmd_vm_run(").unwrap()..];
        let run = &run[..run.find("\n}\n").unwrap()];
        assert!(run.contains("PastaNetwork::isolated("));
        assert!(!run.contains(concat!("PastaNetwork", "::new(")));
        let start = source.find("async fn boot_raw_vm(").unwrap();
        let body = &source[start..start + source[start..].find("pub(crate) struct RawVm").unwrap()];
        let post_start = body.find(".post_start(holder_pid)").unwrap();
        let helper = body.find("NsHelper::start(").unwrap();
        let boot = body.find("backend.boot()").unwrap();
        assert!(post_start < helper && helper < boot);
        for absent in ["set_vsock", "boot_plan", "apply_launch_config", "bootplan"] {
            assert!(!body.contains(absent), "the raw boot calls {absent}");
        }
    }
}
