//! Cloud Hypervisor backend (#632 P1).
//!
//! Implements [`Hypervisor`](super::Hypervisor) for Cloud Hypervisor. CH has a batch
//! create API (one `vm.create` then `vm.boot`), so the `configure_*` trait methods buffer
//! a [`PendingConfig`] and [`Hypervisor::boot`] translates it to a [`api::VmConfig`] and
//! performs create + boot. [`Hypervisor::spawn`] launches `cloud-hypervisor --api-socket`
//! (so the process PID + control socket exist before configuration), reusing the shared
//! [`crate::utils::install_namespace_pre_exec`] namespace isolation.
//!
//! Cold boot (no MMDS → boot plan over vsock, handled by the orchestration) plus explicit
//! snapshot create / restore-clone (P2): the snapshot path drives `vm.snapshot` /
//! `--restore` and is reached from `commands::common`. ARM64 boot specifics: the kernel must
//! be the `Image` (PE) format (fcvm's asset already is), the console is virtio `hvc0` (not
//! the PL011 `ttyAMA0`), and `pci=off` must be removed.

pub mod api;

use anyhow::{anyhow, bail, Context, Result};
use std::collections::VecDeque;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use self::api::{
    BalloonConfig, ChClient, ConsoleConfig, CpusConfig, DiskConfig, MemoryConfig, NetConfig,
    PayloadConfig, RngConfig, VmConfig, VsockConfig,
};
use super::{Backend, Capabilities, DriveSpec, Hypervisor, NetIfaceSpec, PmemSpec, ProcessSpec};
use crate::utils::{install_namespace_pre_exec, spawn_streaming, NamespaceParams};

/// Guest CID for the host↔guest vsock device (host is always CID 2).
const GUEST_CID: u32 = 3;
/// Total wait for the api-socket to accept + ping (RETRY_COUNT * RETRY_DELAY).
const SOCKET_WAIT_RETRY_COUNT: u32 = 500;
const SOCKET_WAIT_RETRY_DELAY: Duration = Duration::from_millis(10);
const STDERR_TAIL_LINES: usize = 10;
/// Upper bound on waiting for the stderr reader to reach EOF after the VMM
/// exits. The wait ends on EOF; this only bounds an inherited, still-open pipe.
const STDERR_EOF_TIMEOUT: Duration = Duration::from_secs(2);

/// Buffered VM configuration accumulated by the `configure_*` methods, applied at boot.
#[derive(Default)]
struct PendingConfig {
    kernel: Option<PathBuf>,
    initramfs: Option<PathBuf>,
    cmdline: String,
    vcpus: u8,
    mem_mib: u32,
    disks: Vec<DiskConfig>,
    net: Vec<NetConfig>,
    vsock: Option<VsockConfig>,
    entropy: bool,
    balloon_mib: Option<u32>,
}

/// Cloud Hypervisor implementation of the [`Hypervisor`](super::Hypervisor) trait.
pub struct CloudHypervisorBackend {
    vm_id: String,
    vm_name: Option<String>,
    api_socket: PathBuf,
    log_path: Option<PathBuf>,
    process: Option<Child>,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
    /// Reader task for the child's stderr pipe, awaited (bounded) before rendering
    /// `stderr_tail` on a launch failure so the tail is complete.
    stderr_reader: Option<JoinHandle<()>>,
    client: Option<ChClient>,
    pending: PendingConfig,
    vsock_path: Option<PathBuf>,
    /// Namespace/mount isolation, retained across re-spawns. A guest reboot relaunches
    /// via [`Hypervisor::spawn`] with a minimal spec (binary + args only — see
    /// `commands::podman::run_vm_loop`), so the isolation captured on the FIRST spawn
    /// must persist or the relaunched VMM would run outside its namespaces. Mirrors how
    /// the Firecracker backend's [`VmManager`](crate::firecracker::VmManager) retains
    /// these fields.
    namespace: NamespaceParams,
    /// The live console tail ([`ConsoleTail`]), tracked so a reboot relaunch (or
    /// [`Hypervisor::reap`]) finishes the previous one instead of leaking it. The
    /// relaunch reuses the VM dir and keeps `ch-console.log`, so an orphaned tail would not
    /// self-exit; finishing it also logs what it had not read yet.
    console_tail: Option<ConsoleTail>,
    /// Guest console (hvc0) lines observed by the console tail since spawn
    /// (see [`Hypervisor::console_line_counter`]).
    console_lines: Arc<std::sync::atomic::AtomicU64>,
    /// One event reader per VMM child. It must finish before the API path is reused.
    reboot_monitor: Option<JoinHandle<Result<()>>>,
}

impl CloudHypervisorBackend {
    /// Create a backend that will manage a Cloud Hypervisor process whose REST API
    /// listens on `api_socket`.
    pub fn new(vm_id: String, api_socket: PathBuf, log_path: Option<PathBuf>) -> Self {
        Self {
            namespace: NamespaceParams {
                vm_id: vm_id.clone(),
                ..Default::default()
            },
            vm_id,
            vm_name: None,
            api_socket,
            log_path,
            process: None,
            stderr_tail: Arc::new(Mutex::new(VecDeque::new())),
            stderr_reader: None,
            client: None,
            pending: PendingConfig::default(),
            vsock_path: None,
            console_tail: None,
            console_lines: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            reboot_monitor: None,
        }
    }

    fn client(&self) -> Result<&ChClient> {
        self.client.as_ref().context("Cloud Hypervisor not started")
    }

    async fn stop_reboot_monitor(&mut self) {
        // Keep ownership across await: wait() can be cancelled by the VM loop.
        if let Some(monitor) = self.reboot_monitor.as_mut() {
            monitor.abort();
            let _ = monitor.await;
        }
        self.reboot_monitor = None;
    }

    /// Merge a spawn spec into the retained namespace isolation: only fields the spec
    /// actually provides overwrite the retained values. A guest reboot relaunches via
    /// [`Hypervisor::spawn`] with a minimal spec (binary + args only), so this preserves
    /// the isolation captured on the first spawn instead of dropping it (codex #632 P1).
    fn merge_spec_namespace(&mut self, spec: &ProcessSpec) {
        if let Some(id) = &spec.namespace_id {
            self.namespace.namespace_id = Some(id.clone());
        }
        if let Some(path) = &spec.user_namespace_path {
            self.namespace.user_namespace_path = Some(path.clone());
        }
        if let Some(path) = &spec.net_namespace_path {
            self.namespace.net_namespace_path = Some(path.clone());
        }
        if let Some(redirects) = &spec.mount_redirects {
            self.namespace.mount_redirects = Some(redirects.clone());
        }
    }

    /// Guest console log path in the VM dir (derived from the VMM log path), tailed into
    /// fcvm's tracing logs by [`spawn`](Self::spawn).
    fn console_path(&self) -> Option<PathBuf> {
        self.log_path
            .as_ref()
            .map(|p| p.with_file_name("ch-console.log"))
    }

    /// Translate Firecracker-style boot args to a Cloud Hypervisor cmdline: CH's default
    /// console is virtio `hvc0` (not the PL011 `ttyS0`/`ttyAMA0`) and it puts virtio
    /// devices on PCI, so `pci=off` must be dropped.
    ///
    /// Also appends `fcvm_shutdown=acpi`: on x86_64 fc-agent's only way to make
    /// Firecracker exit is a triple fault (`reboot -f` under `reboot=t`), but Cloud
    /// Hypervisor treats a triple fault as a guest-initiated RESET and reboots the VM
    /// in-process — the guest boot-loops and fcvm's `vm_manager.wait()` never returns.
    /// The token tells fc-agent this VMM honors ACPI/PSCI power-off (CH exits its
    /// process on either), so it must power off instead of triple-faulting.
    ///
    /// Also appends `net.ifnames=0`, as the Firecracker PCI profiles do: CH puts the guest's
    /// virtio-net NIC on PCI, which udev renames to `enp0s<slot>` under predictable naming,
    /// while fc-agent and fcvm configure `eth0` (its IPv6 address and route, the gateway's
    /// neighbour entry, the snapshot gate) and the rootfs's networkd unit matches
    /// `Name=eth0` (#806).
    fn ch_cmdline(fc_boot_args: &str, runtime_boot_args: &str) -> String {
        let combined = if runtime_boot_args.is_empty() {
            fc_boot_args.to_string()
        } else {
            format!("{fc_boot_args} {runtime_boot_args}")
        };
        // Token-based (not substring) so we only rewrite/drop whole kernel args, never an
        // embedded value: map the serial console to virtio hvc0 and drop pci=off (CH puts
        // virtio devices on PCI).
        let mut tokens: Vec<&str> = combined
            .split_whitespace()
            .filter_map(|tok| match tok {
                "pci=off" => None,
                "console=ttyS0" | "console=ttyAMA0" => Some("console=hvc0"),
                other => Some(other),
            })
            .collect();
        if !tokens.contains(&"net.ifnames=0") {
            tokens.push("net.ifnames=0");
        }
        tokens.push("fcvm_shutdown=acpi");
        tokens.join(" ")
    }

    /// Wait for the api-socket to accept connections and answer a ping, failing fast if
    /// the process exits first (mirrors the Firecracker socket wait).
    async fn wait_for_api(&mut self) -> Result<()> {
        use tokio::time::sleep;
        let probe = ChClient::new(self.api_socket.clone());
        for _ in 0..SOCKET_WAIT_RETRY_COUNT {
            if tokio::net::UnixStream::connect(&self.api_socket)
                .await
                .is_ok()
                && probe.ping().await.is_ok()
            {
                return Ok(());
            }
            if let Some(status) = self.try_wait()? {
                // Wait for the stderr reader to hit EOF (the exit closed the write
                // end) so the error carries everything the VMM printed.
                crate::utils::wait_for_stderr_eof(&mut self.stderr_reader, STDERR_EOF_TIMEOUT)
                    .await;
                bail!(
                    "Cloud Hypervisor exited with {} before its API socket became ready{}",
                    status,
                    self.stderr_tail_message()
                );
            }
            sleep(SOCKET_WAIT_RETRY_DELAY).await;
        }
        bail!(
            "Cloud Hypervisor API socket not ready after {} seconds",
            SOCKET_WAIT_RETRY_COUNT as u64 * SOCKET_WAIT_RETRY_DELAY.as_millis() as u64 / 1000
        )
    }

    fn stderr_tail_message(&self) -> String {
        let lines: Vec<String> = self
            .stderr_tail
            .lock()
            .map(|tail| tail.iter().cloned().collect())
            .unwrap_or_default();
        if lines.is_empty() {
            "; no stderr captured (run with RUST_LOG=debug)".to_string()
        } else {
            format!("; last stderr output:\n  {}", lines.join("\n  "))
        }
    }

    /// Build the Cloud Hypervisor VmConfig from the buffered `configure_*` state.
    fn build_vm_config(&self) -> Result<VmConfig> {
        let kernel = self
            .pending
            .kernel
            .as_ref()
            .context("no kernel configured for Cloud Hypervisor boot")?;
        Ok(VmConfig {
            cpus: CpusConfig {
                boot_vcpus: self.pending.vcpus,
                max_vcpus: self.pending.vcpus,
            },
            memory: MemoryConfig {
                size: self.pending.mem_mib as u64 * 1024 * 1024,
                shared: false,
                thp: false,
            },
            payload: PayloadConfig {
                kernel: kernel.display().to_string(),
                cmdline: self.pending.cmdline.clone(),
                initramfs: self
                    .pending
                    .initramfs
                    .as_ref()
                    .map(|p| p.display().to_string()),
            },
            disks: self.pending.disks.clone(),
            net: self
                .pending
                .net
                .iter()
                .map(|n| NetConfig {
                    tap: n.tap.clone(),
                    mac: n.mac.clone(),
                })
                .collect(),
            vsock: self.pending.vsock.as_ref().map(|v| VsockConfig {
                cid: v.cid,
                socket: v.socket.clone(),
            }),
            // Virtio console (hvc0) to a file in the VM dir, which spawn() tails into
            // fcvm's tracing logs. CH's "Tty" mode needs a controlling terminal, which a
            // piped child does not have, so file + tail is the portable equivalent of
            // Firecracker's serial-to-stdout capture.
            console: match self.console_path() {
                Some(p) => ConsoleConfig {
                    mode: "File".to_string(),
                    file: Some(p.display().to_string()),
                },
                None => ConsoleConfig {
                    mode: "Off".to_string(),
                    file: None,
                },
            },
            serial: ConsoleConfig {
                mode: "Null".to_string(),
                file: None,
            },
            rng: self.pending.entropy.then(|| RngConfig {
                src: "/dev/urandom".to_string(),
            }),
            balloon: self.pending.balloon_mib.map(|mib| BalloonConfig {
                size: mib as u64 * 1024 * 1024,
                deflate_on_oom: true,
            }),
        })
    }
}

#[async_trait::async_trait]
impl Hypervisor for CloudHypervisorBackend {
    fn backend(&self) -> Backend {
        Backend::CloudHypervisor
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::cloud_hypervisor()
    }

    async fn spawn(&mut self, spec: &ProcessSpec) -> Result<()> {
        self.stop_reboot_monitor().await;
        // A reboot relaunch re-enters spawn() with a minimal spec (binary + args). Update
        // retained state only when the spec provides a value, so the namespace isolation
        // and name captured on the first spawn persist across reboots (mirrors the
        // Firecracker backend's VmManager). The buffered device config, however, is
        // rebuilt fresh each boot by the orchestration's configure_* replay, so clear it
        // here to avoid sending duplicate disks/net devices to the next vm.create.
        self.pending = PendingConfig::default();
        // Drop any stderr captured from a prior child so a spawn-failure message reflects
        // only the new process.
        if let Ok(mut tail) = self.stderr_tail.lock() {
            tail.clear();
        }
        if let Some(name) = &spec.vm_name {
            self.vm_name = Some(name.clone());
        }
        self.merge_spec_namespace(spec);
        // A reboot relaunch: log the rest of the old guest's console before this process can
        // write the file (Cloud Hypervisor truncates it when it creates the VM), then empty it,
        // so the new tail starts at the new guest's first byte and repeats nothing.
        if let Some(old) = self.console_tail.take() {
            old.finish();
            if let Some(path) = self.console_path() {
                if let Err(error) = std::fs::OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .and_then(|file| file.set_len(0))
                {
                    warn!(path = %path.display(), %error, "could not empty the guest console file for the relaunch");
                }
            }
        }

        let _ = std::fs::remove_file(&self.api_socket);

        let mut cmd = Command::new(&spec.binary);
        cmd.arg("--api-socket").arg(&self.api_socket);
        let (events, child_events) = UnixStream::pair().context("creating CH event socketpair")?;
        events.set_nonblocking(true)?;
        let events = tokio::net::UnixStream::from_std(events)?;
        let event_fd = child_events.as_raw_fd();
        cmd.arg("--event-monitor").arg(format!("fd={event_fd}"));
        if let Some(log_path) = &self.log_path {
            cmd.arg("--log-file").arg(log_path);
            cmd.arg("-v");
        }
        if let Some(extra) = &spec.extra_args {
            for arg in extra.split_whitespace() {
                cmd.arg(arg);
            }
        }

        // Clear CLOEXEC only in this child. Changing it in the parent would let
        // unrelated concurrent spawns inherit the event socket. Namespace setup
        // remains the last pre_exec so its post-setns PDEATHSIG is preserved.
        // SAFETY: fcntl is async-signal-safe, and child_events owns the fd until spawn returns.
        unsafe {
            cmd.pre_exec(move || {
                if libc::fcntl(event_fd, libc::F_SETFD, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        install_namespace_pre_exec(&mut cmd, &self.namespace)?;

        let stderr_tail = Arc::clone(&self.stderr_tail);
        let spawned = spawn_streaming(cmd, move |line, is_stderr| {
            let clean = line.trim_end();
            if is_stderr {
                if let Ok(mut tail) = stderr_tail.lock() {
                    if tail.len() >= STDERR_TAIL_LINES {
                        tail.pop_front();
                    }
                    tail.push_back(clean.to_string());
                }
                debug!(target: "cloud-hypervisor", "{}", clean);
            } else {
                log_console_line(clean);
            }
        })
        .context("spawning Cloud Hypervisor process")?;
        drop(child_events);

        self.process = Some(spawned.child);
        self.stderr_reader = Some(spawned.stderr_reader);
        self.wait_for_api().await?;
        self.client = Some(ChClient::new(self.api_socket.clone()));
        let client = self.client()?.clone();
        self.reboot_monitor = Some(tokio::spawn(async move {
            if read_reboot_event(BufReader::new(events)).await? {
                // The guest's vsock notification is early intent, before shutdown
                // writeback. CH emits this event only when it handles the actual
                // reset. Exit the VMM here so fcvm consumes intent and cold-relaunches.
                info!("Cloud Hypervisor guest reset, shutting down VMM for host relaunch");
                client.shutdown_vmm().await?;
            }
            Ok(())
        }));

        // Tail the guest console (hvc0 → file) into fcvm's tracing logs, mirroring the
        // Firecracker serial-to-stdout capture. A prior tail was finished above, before this
        // process started. Track it so start_kill() finishes it too.
        if let Some(cpath) = self.console_path() {
            self.console_tail = Some(ConsoleTail::start(cpath, Arc::clone(&self.console_lines)));
        }
        Ok(())
    }

    fn pid(&self) -> Result<u32> {
        self.process
            .as_ref()
            .and_then(|p| p.id())
            .ok_or_else(|| anyhow!("Cloud Hypervisor process not running"))
    }

    fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
        match self.process.as_mut() {
            Some(p) => match p.try_wait().context("checking Cloud Hypervisor status")? {
                Some(status) => {
                    if let Some(monitor) = &self.reboot_monitor {
                        monitor.abort();
                    }
                    self.process = None;
                    Ok(Some(status))
                }
                None => Ok(None),
            },
            None => bail!("Cloud Hypervisor process not running"),
        }
    }

    async fn wait(&mut self) -> Result<ExitStatus> {
        let process = self
            .process
            .as_mut()
            .context("Cloud Hypervisor process not running")?;
        let mut monitor_failure = None;
        let status = if let Some(monitor) = self.reboot_monitor.as_mut() {
            tokio::select! {
                biased;
                status = process.wait() => status,
                result = monitor => {
                    self.reboot_monitor = None;
                    if let Err(error) = result.context("CH reboot monitor task failed").and_then(|r| r) {
                        warn!(%error, "CH reboot monitor failed, terminating VMM");
                        // The caller treats any wait() return as child termination.
                        // Never report this failure while the VMM can still be alive.
                        process.start_kill().context("terminating CH after event monitor failure")?;
                        monitor_failure = Some(error);
                    }
                    process.wait().await
                }
            }
        } else {
            process.wait().await
        }
        .context("waiting for Cloud Hypervisor")?;
        self.stop_reboot_monitor().await;
        self.process = None;
        if let Some(error) = monitor_failure {
            return Err(error);
        }
        Ok(status)
    }

    fn start_kill(&mut self) -> Result<()> {
        if let Some(monitor) = &self.reboot_monitor {
            monitor.abort();
        }
        if let Some(ref mut p) = self.process {
            info!(vm_id = %self.vm_id, "SIGKILL to Cloud Hypervisor process");
            p.start_kill()
                .context("sending SIGKILL to Cloud Hypervisor")?;
        }
        Ok(())
    }

    async fn reap(&mut self) {
        self.stop_reboot_monitor().await;
        if let Some(mut p) = self.process.take() {
            let _ = p.wait().await;
        }
        // The VMM is gone, so nothing more reaches the console file: log what the tail had not
        // read yet, a last line without its newline included.
        if let Some(tail) = self.console_tail.take() {
            tail.finish();
        }
    }

    async fn stream_console(&self, _console_path: &Path) -> Result<mpsc::Receiver<String>> {
        // The guest console (hvc0) goes to a file in the VM dir, which tail_console_to_tracing
        // follows into fcvm's logs. There is no stream to return.
        let (_tx, rx) = mpsc::channel(1);
        Ok(rx)
    }

    fn console_line_counter(&self) -> Arc<std::sync::atomic::AtomicU64> {
        Arc::clone(&self.console_lines)
    }

    async fn apply_launch_config(
        &mut self,
        config: &crate::firecracker::FirecrackerConfig,
        runtime_boot_args: &str,
        _track_dirty: bool,
    ) -> Result<()> {
        self.pending.kernel = Some(config.boot_source.kernel_image_path.clone());
        self.pending.initramfs = Some(config.boot_source.initrd_path.clone());
        self.pending.cmdline = Self::ch_cmdline(&config.boot_source.boot_args, runtime_boot_args);
        self.pending.vcpus = config.machine_config.vcpu_count;
        self.pending.mem_mib = config.machine_config.mem_size_mib;
        // Root + any drives from the launch config (rootfs is the first disk → /dev/vda).
        for drive in &config.drives {
            self.pending.disks.push(DiskConfig {
                path: drive.path_on_host.display().to_string(),
                readonly: drive.is_read_only,
                image_type: "Raw".to_string(),
            });
        }
        Ok(())
    }

    async fn add_drive(&mut self, drive: &DriveSpec) -> Result<()> {
        self.pending.disks.push(DiskConfig {
            path: drive.path_on_host.display().to_string(),
            readonly: drive.is_read_only,
            image_type: "Raw".to_string(),
        });
        Ok(())
    }

    async fn add_pmem(&mut self, pmem: &PmemSpec) -> Result<()> {
        anyhow::bail!(
            "--pmem is not supported with --hypervisor cloud-hypervisor (device {} for {}); \
             use the Firecracker backend",
            pmem.id,
            pmem.path_on_host.display()
        )
    }

    async fn add_network_interface(&mut self, iface: &NetIfaceSpec) -> Result<()> {
        self.pending.net.push(NetConfig {
            tap: iface.host_dev_name.clone(),
            mac: iface.guest_mac.clone(),
        });
        Ok(())
    }

    async fn configure_metadata_service(&mut self) -> Result<()> {
        // Cloud Hypervisor has no MMDS; the boot plan is delivered over vsock (P0.5).
        Ok(())
    }

    async fn set_vsock(&mut self, guest_cid: u32, uds_path: &Path) -> Result<()> {
        let _ = std::fs::remove_file(uds_path);
        self.vsock_path = Some(uds_path.to_path_buf());
        self.pending.vsock = Some(VsockConfig {
            cid: guest_cid,
            socket: uds_path.display().to_string(),
        });
        Ok(())
    }

    async fn publish_boot_plan(&mut self, _plan: serde_json::Value) -> Result<()> {
        // Never reached for Cloud Hypervisor: the orchestration delivers the plan over the
        // vsock boot-plan listener (capabilities().native_metadata_service == false).
        warn!("publish_boot_plan called on Cloud Hypervisor backend (expected vsock boot plan)");
        Ok(())
    }

    async fn add_entropy_device(&mut self) -> Result<()> {
        self.pending.entropy = true;
        Ok(())
    }

    async fn add_balloon(&mut self, device: crate::firecracker::BalloonDevice) -> Result<()> {
        anyhow::ensure!(
            !device.free_page_reporting,
            "--free-page-reporting needs Firecracker: fcvm's Cloud Hypervisor backend attaches \
             a balloon without free page reporting"
        );
        self.pending.balloon_mib = Some(device.target_mib);
        Ok(())
    }

    async fn boot(&mut self) -> Result<()> {
        let config = self.build_vm_config()?;
        let client = self.client()?;
        client.create_vm(&config).await.context("vm.create")?;
        client.boot_vm().await.context("vm.boot")?;
        Ok(())
    }

    async fn pause(&self) -> Result<()> {
        self.client()?.pause_vm().await
    }

    async fn resume(&self) -> Result<()> {
        self.client()?.resume_vm().await
    }

    fn vsock_socket_path(&self) -> Option<&Path> {
        self.vsock_path.as_deref()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

impl Drop for CloudHypervisorBackend {
    fn drop(&mut self) {
        if let Some(monitor) = &self.reboot_monitor {
            monitor.abort();
        }
    }
}

/// CH writes pretty-printed JSON objects separated by a blank line, not JSONL.
async fn read_reboot_event(reader: impl AsyncBufRead + Unpin) -> Result<bool> {
    #[derive(serde::Deserialize)]
    struct Event {
        source: String,
        event: String,
    }

    let mut lines = reader.lines();
    let mut frame = String::new();
    while let Some(line) = lines.next_line().await.context("reading CH events")? {
        if line.is_empty() {
            if frame.is_empty() {
                continue;
            }
            let event: Event = serde_json::from_str(&frame).context("decoding CH event")?;
            if event.source == "vm" && event.event == "rebooting" {
                return Ok(true);
            }
            frame.clear();
        } else {
            frame.push_str(&line);
            frame.push('\n');
        }
    }
    anyhow::ensure!(frame.is_empty(), "CH event stream ended within an event");
    Ok(false)
}

/// The default guest CID Cloud Hypervisor uses for the host↔guest vsock device.
pub const fn default_guest_cid() -> u32 {
    GUEST_CID
}

/// Log one line of the guest console: a line fcvm keeps above DEBUG under fcvm's own targets
/// (`log_console_line_under_fcvm`), the rest at DEBUG under `cloud-hypervisor`.
fn log_console_line(clean: &str) {
    if !crate::utils::log_console_line_under_fcvm(clean) {
        debug!(target: "cloud-hypervisor", "{}", clean);
    }
}

/// The most bytes of one console line the tail holds while it waits for the line's newline. A
/// guest that writes to hvc0 without newlines must not grow fcvm's memory without limit; a
/// kernel record is under 1 KiB, so this keeps every one of them whole. A longer run is logged
/// in pieces of this size.
const CONSOLE_LINE_CAP: usize = 64 << 10;

/// What the console tail has consumed of the console file: the byte offset it has read to, and
/// the start of a line whose newline it has not read yet. Shared between the tail's task and the
/// backend, which finishes the tail when it stops it ([`ConsoleTail::finish`]).
#[derive(Default)]
struct ConsoleTailState {
    pos: u64,
    pending: Vec<u8>,
    /// Set by [`ConsoleTail::finish`]: the task's read in flight, if any, is discarded.
    finished: bool,
}

impl ConsoleTailState {
    /// Take `bytes` read at `self.pos`, and log every whole line in `pending` plus them.
    fn consume(&mut self, bytes: &[u8], console_lines: &std::sync::atomic::AtomicU64) {
        self.pos += bytes.len() as u64;
        self.pending.extend_from_slice(bytes);
        let mut start = 0;
        while let Some(newline) = self.pending[start..].iter().position(|&b| b == b'\n') {
            let end = start + newline;
            log_console_bytes(&self.pending[start..end], console_lines);
            start = end + 1;
        }
        self.pending.drain(..start);
        while self.pending.len() >= CONSOLE_LINE_CAP {
            let piece: Vec<u8> = self.pending.drain(..CONSOLE_LINE_CAP).collect();
            log_console_bytes(&piece, console_lines);
        }
    }

    /// Log what is left of an unfinished line.
    fn flush(&mut self, console_lines: &std::sync::atomic::AtomicU64) {
        if !self.pending.is_empty() {
            let rest = std::mem::take(&mut self.pending);
            log_console_bytes(&rest, console_lines);
        }
    }
}

fn log_console_bytes(line: &[u8], console_lines: &std::sync::atomic::AtomicU64) {
    console_lines.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    log_console_line(String::from_utf8_lossy(line).trim_end());
}

/// A running console tail ([`tail_console_to_tracing`]) and what it has consumed, so the backend
/// can finish it when it stops it.
struct ConsoleTail {
    task: JoinHandle<()>,
    state: Arc<Mutex<ConsoleTailState>>,
    path: PathBuf,
    console_lines: Arc<std::sync::atomic::AtomicU64>,
}

impl ConsoleTail {
    fn start(path: PathBuf, console_lines: Arc<std::sync::atomic::AtomicU64>) -> Self {
        let state = Arc::new(Mutex::new(ConsoleTailState::default()));
        let task = tokio::spawn(tail_console_to_tracing(
            path.clone(),
            Arc::clone(&state),
            Arc::clone(&console_lines),
        ));
        Self {
            task,
            state,
            path,
            console_lines,
        }
    }

    /// Stop the tail and log what it had not logged yet: the rest of the file, and a last line
    /// without its newline. Called once the VMM has exited (`reap`, and a relaunch's `spawn`), so
    /// nothing more is written. A read the task had in flight is discarded (`finished`), so
    /// nothing is logged twice.
    fn finish(self) {
        use std::io::{Read, Seek};
        self.task.abort();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.finished {
            return;
        }
        state.finished = true;
        if let Ok(mut file) = std::fs::File::open(&self.path) {
            if file.seek(std::io::SeekFrom::Start(state.pos)).is_ok() {
                let mut buf = vec![0u8; CONSOLE_LINE_CAP];
                while let Ok(n @ 1..) = file.read(&mut buf) {
                    state.consume(&buf[..n], &self.console_lines);
                }
            }
        }
        state.flush(&self.console_lines);
    }
}

/// Follow the Cloud Hypervisor guest console file and emit each new line to fcvm's
/// tracing logs (the portable equivalent of Firecracker streaming its serial to stdout)
/// through [`log_console_line`]. A line read in pieces across polls is held until its newline
/// arrives ([`ConsoleTailState::consume`]), so it is logged, and classified, whole (#1107). The
/// loop exits once the file has been gone for a few seconds (the VM dir was cleaned up), so it
/// never outlives its VM. Each line also bumps `console_lines` (dead-console detection after
/// restore — see [`Hypervisor::console_line_counter`]).
async fn tail_console_to_tracing(
    path: PathBuf,
    state: Arc<Mutex<ConsoleTailState>>,
    console_lines: Arc<std::sync::atomic::AtomicU64>,
) {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let lock = || {
        state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    };

    // Wait for Cloud Hypervisor to create the console file at boot.
    for _ in 0..SOCKET_WAIT_RETRY_COUNT {
        if path.exists() {
            break;
        }
        tokio::time::sleep(SOCKET_WAIT_RETRY_DELAY).await;
    }

    let mut buf = vec![0u8; CONSOLE_LINE_CAP];
    let mut missing = 0u32;
    loop {
        let from = {
            let state = lock();
            if state.finished {
                return;
            }
            state.pos
        };
        match tokio::fs::File::open(&path).await {
            Ok(mut file) => {
                missing = 0;
                if file.seek(std::io::SeekFrom::Start(from)).await.is_ok() {
                    loop {
                        let n = match file.read(&mut buf).await {
                            Ok(0) | Err(_) => break, // caught up; poll again after a short sleep
                            Ok(n) => n,
                        };
                        let mut state = lock();
                        if state.finished {
                            return;
                        }
                        state.consume(&buf[..n], &console_lines);
                    }
                }
            }
            Err(_) => {
                // Console file gone (VM dir cleaned up). Stop after a grace window.
                missing += 1;
                if missing > 30 {
                    let mut state = lock();
                    if !state.finished {
                        state.finished = true;
                        state.flush(&console_lines);
                    }
                    return;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[cfg(test)]
mod tests {
    /// fc-agent's console lines reach a log filtered to fcvm's own targets (#1104); the other
    /// console lines stay under `cloud-hypervisor`.
    #[test]
    fn cloud_hypervisor_logs_agent_console_lines_under_fcvm() {
        use crate::utils::log_capture::logged;
        let all = || {
            super::log_console_line("[fc-agent] run_agent starting");
            super::log_console_line("[    1.234567] virtio_net virtio1 eth0: link up");
        };
        let info = logged("fcvm=info", all);
        assert_eq!(info.matches("run_agent starting").count(), 1, "{info:?}");
        assert!(!info.contains("virtio_net"), "{info:?}");
        assert!(logged("cloud-hypervisor=debug", all).contains("virtio_net"));
    }

    /// A guest kernel's panic line in the Cloud Hypervisor console file reaches fcvm's default
    /// log once, at WARN under fcvm::guest, and not also under the console's own target.
    #[test]
    fn cloud_hypervisor_console_logs_a_guest_kernel_panic_line_once_at_warn() {
        use crate::utils::log_capture::logged;
        let line = "[    9.814619] Kernel panic - not syncing: sysrq triggered crash";
        let log = logged("warn", || super::log_console_line(line));
        assert_eq!(
            log.matches("Kernel panic - not syncing").count(),
            1,
            "{log:?}"
        );
        let console_target = logged("cloud-hypervisor=trace", || super::log_console_line(line));
        assert!(
            !console_target.contains("Kernel panic"),
            "{console_target:?}"
        );
    }

    /// fc-agent's FATAL and WARNING lines reach a log filtered to fcvm's own targets, once, at
    /// WARN under fcvm::guest (#1080).
    #[test]
    fn cloud_hypervisor_console_logs_fc_agent_alerts_under_fcvm() {
        use crate::utils::log_capture::logged;
        let fatal = "[fc-agent] FATAL: clone restore failed closed (epoch=e): boom";
        for filter in ["warn", "fcvm=info"] {
            let log = logged(filter, || super::log_console_line(fatal));
            assert_eq!(
                log.matches("FATAL: clone restore").count(),
                1,
                "{filter}: {log:?}"
            );
        }
        let console_target = logged("cloud-hypervisor=trace", || super::log_console_line(fatal));
        assert!(!console_target.contains("FATAL"), "{console_target:?}");
    }

    use super::*;

    /// A reboot relaunch finishes the old guest's console tail before it starts the new Cloud
    /// Hypervisor process, which truncates the file it shares with the old guest. Started
    /// after, as the tail used to be, the old guest's unread lines were lost.
    #[test]
    fn a_relaunch_finishes_the_old_console_tail_before_the_new_vmm_starts() {
        let source = include_str!("mod.rs");
        let spawn = &source[source
            .find("    async fn spawn(&mut self, spec: &ProcessSpec)")
            .unwrap()..];
        let finish = spawn
            .find("old.finish()")
            .expect("spawn finishes the old tail");
        let command = spawn.find("Command::new(&spec.binary)").unwrap();
        assert!(
            finish < command,
            "the old tail is finished after the new VMM starts"
        );
    }

    /// The tail is finished once the VMM has been reaped, not when it is signalled: a write the
    /// VMM had in flight when it was killed lands after the signal, and a tail finished then
    /// would have reached the end of the file before it.
    #[test]
    fn the_console_tail_is_finished_after_the_vmm_is_reaped() {
        let source = include_str!("mod.rs");
        let body = |name: &str| {
            let start = source.find(name).unwrap();
            &source[start..start + source[start..].find("\n    }\n").unwrap()]
        };
        let start_kill = body("    fn start_kill(&mut self) -> Result<()> {");
        assert!(!start_kill.contains("finish()"), "{start_kill}");
        let reap = body("    async fn reap(&mut self) {");
        let waited = reap.find("p.wait().await").unwrap();
        let finished = reap.find("tail.finish()").expect("reap finishes the tail");
        assert!(waited < finished, "{reap}");
    }

    /// Run `body` on a current-thread runtime under a subscriber that keeps everything, and
    /// return what it logged. The tail's task runs on this thread, so its lines are captured.
    fn logged_by_tail(body: impl FnOnce(&tokio::runtime::Runtime)) -> String {
        crate::utils::log_capture::logged("trace", || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            body(&runtime);
        })
    }

    /// Wait until the tail has consumed `bytes` of its file.
    async fn tail_reaches(tail: &ConsoleTail, bytes: u64) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while tail.state.lock().unwrap().pos < bytes {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the tail did not read {bytes} bytes within 10 s"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn append(path: &Path, bytes: &[u8]) {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap()
            .write_all(bytes)
            .unwrap();
    }

    /// A kernel panic line Cloud Hypervisor writes in two pieces, with a poll of the tail in
    /// between, is logged once, whole, as a crash headline at WARN, and counted as one line.
    /// The tail used to log each read as a line, so the headline was split and neither piece
    /// was classified as one (#1107).
    #[test]
    fn a_line_written_in_two_pieces_is_logged_whole() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ch-console.log");
        let first = b"[    9.814619] Kernel panic - not ";
        let rest = b"syncing: sysrq triggered crash\n";
        let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let log = logged_by_tail(|runtime| {
            runtime.block_on(async {
                append(&path, first);
                let tail = ConsoleTail::start(path.clone(), Arc::clone(&counter));
                tail_reaches(&tail, first.len() as u64).await;
                append(&path, rest);
                tail_reaches(&tail, (first.len() + rest.len()) as u64).await;
                tail.finish();
            })
        });
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "{log}"
        );
        let headline: Vec<&str> = log
            .lines()
            .filter(|line| line.contains("Kernel panic"))
            .collect();
        assert_eq!(headline.len(), 1, "{log}");
        assert!(
            headline[0].contains("WARN")
                && headline[0].contains("fcvm::guest")
                && headline[0].contains("Kernel panic - not syncing: sysrq triggered crash"),
            "{log}"
        );
    }

    /// A guest that writes without newlines cannot grow the tail's buffer past
    /// CONSOLE_LINE_CAP: that much is logged as one line without waiting for a newline.
    #[test]
    fn a_line_without_a_newline_is_logged_at_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ch-console.log");
        let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let log = logged_by_tail(|runtime| {
            runtime.block_on(async {
                append(&path, &vec![b'x'; CONSOLE_LINE_CAP]);
                let tail = ConsoleTail::start(path.clone(), Arc::clone(&counter));
                tail_reaches(&tail, CONSOLE_LINE_CAP as u64).await;
                assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 1);
                assert!(tail.state.lock().unwrap().pending.is_empty());
                tail.finish();
            })
        });
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "finishing logs nothing more"
        );
        assert!(
            log.contains(&"x".repeat(CONSOLE_LINE_CAP)),
            "the line is logged whole"
        );
    }

    /// A last record without its newline, written just before the VMM is stopped, is logged
    /// once by the stop, whether or not the tail had read it yet.
    #[test]
    fn stopping_the_tail_logs_a_last_line_without_a_newline_once() {
        for wait_for_the_tail in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("ch-console.log");
            let words = b"[fc-agent] the last words";
            let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let log = logged_by_tail(|runtime| {
                runtime.block_on(async {
                    append(&path, b"[fc-agent] booted\n");
                    let tail = ConsoleTail::start(path.clone(), Arc::clone(&counter));
                    append(&path, words);
                    if wait_for_the_tail {
                        tail_reaches(&tail, 18 + words.len() as u64).await;
                    }
                    tail.finish();
                })
            });
            assert_eq!(
                log.matches("the last words").count(),
                1,
                "waited={wait_for_the_tail}: {log}"
            );
            assert_eq!(
                counter.load(std::sync::atomic::Ordering::Relaxed),
                2,
                "waited={wait_for_the_tail}: {log}"
            );
        }
    }

    #[tokio::test]
    async fn reboot_monitor_cleanup_retains_handle_when_cancelled() {
        use std::future::Future;

        let mut be = backend();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        // A started blocking task models a reader still in its current poll:
        // abort requests cancellation but cannot finish the join until it yields.
        be.reboot_monitor = Some(tokio::task::spawn_blocking(move || {
            let _ = started_tx.send(());
            let _ = release_rx.recv();
            Ok(())
        }));
        started_rx.await.unwrap();
        let pending = {
            let mut cleanup = std::pin::pin!(be.stop_reboot_monitor());
            let mut context = std::task::Context::from_waker(std::task::Waker::noop());
            cleanup.as_mut().poll(&mut context).is_pending()
        };
        let retained = be.reboot_monitor.is_some();
        // Release even on a failed assertion, so the fixture cannot strand a worker.
        drop(release_tx);
        be.stop_reboot_monitor().await;
        assert!(pending, "cleanup must wait for the reader to finish");
        assert!(
            retained,
            "cancelled cleanup must retain the reader's join handle"
        );
        assert!(be.reboot_monitor.is_none());
    }

    #[tokio::test]
    async fn reboot_monitor_failure_reaps_child_before_returning() {
        let mut be = backend();
        // The child moves into the backend, so it cannot sit behind a guard of the test's
        // own. `kill_on_drop` covers a test that returns or panics, and the pdeath hook
        // covers a test process that is killed outright, where no drop runs.
        let mut vmm = Command::new("sleep");
        vmm.arg("60").kill_on_drop(true);
        crate::test_child::die_with_the_spawning_thread(vmm.as_std_mut());
        be.process = Some(vmm.spawn().unwrap());
        be.reboot_monitor = Some(tokio::spawn(async { bail!("injected CH event failure") }));
        let result = tokio::time::timeout(Duration::from_secs(2), be.wait()).await;
        let reaped = be.process.is_none();
        be.start_kill().unwrap();
        be.reap().await;
        assert!(result
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("injected CH event failure"));
        assert!(
            reaped,
            "wait must not report a monitor failure with a live VMM child"
        );
    }

    #[tokio::test]
    async fn reboot_monitor_requires_complete_vm_reset_event() {
        let other_events = concat!(
            "{\n  \"source\": \"vmm\",\n  \"event\": \"rebooting\"\n}\n\n",
            "{\n  \"source\": \"vm\",\n  \"event\": \"rebooted\"\n}\n\n",
        );
        assert!(!read_reboot_event(other_events.as_bytes()).await.unwrap());
        let events = format!(
            "{other_events}{{\n  \"source\": \"vm\",\n  \"event\": \"rebooting\",\n  \"properties\": null\n}}\n\n"
        );
        // One-byte reads split both the JSON and its delimiter across reads.
        assert!(
            read_reboot_event(BufReader::with_capacity(1, events.as_bytes()))
                .await
                .unwrap()
        );
        assert!(read_reboot_event(b"{\n  \"source\": \"vm\"".as_slice())
            .await
            .is_err());
        assert!(read_reboot_event(b"not-json\n\n".as_slice()).await.is_err());
    }

    fn backend() -> CloudHypervisorBackend {
        CloudHypervisorBackend::new(
            "vm-test".to_string(),
            PathBuf::from("/tmp/ch-test.sock"),
            None,
        )
    }

    /// fcvm turns free page reporting on only on a Firecracker balloon. `podman run`
    /// refuses the switch for Cloud Hypervisor before anything is set up. This is
    /// the same refusal for any other caller of the trait, so a device that asks
    /// for reporting is never attached without it.
    #[tokio::test]
    async fn a_balloon_with_free_page_reporting_is_refused() {
        use crate::firecracker::BalloonDevice;
        let mut be = backend();
        be.add_balloon(BalloonDevice {
            target_mib: 64,
            free_page_reporting: false,
        })
        .await
        .expect("a balloon without free page reporting");
        assert_eq!(be.pending.balloon_mib, Some(64));

        let error = be
            .add_balloon(BalloonDevice {
                target_mib: 64,
                free_page_reporting: true,
            })
            .await
            .expect_err("a balloon with free page reporting was accepted")
            .to_string();
        assert!(
            error.contains("--free-page-reporting") && error.contains("Cloud Hypervisor"),
            "{error}"
        );
    }

    /// CH madvises guest RAM MADV_HUGEPAGE unless the request says `thp: false`, so an
    /// absent field opts in. See `MemoryConfig::thp` for what that cost on CI.
    #[test]
    fn vm_config_opts_guest_memory_out_of_transparent_hugepages() {
        let mut be = backend();
        be.pending.kernel = Some(PathBuf::from("/boot/Image"));
        let config = serde_json::to_value(be.build_vm_config().unwrap()).unwrap();
        assert_eq!(
            config["memory"]["thp"],
            serde_json::Value::Bool(false),
            "vm.create memory config: {}",
            config["memory"]
        );
    }

    /// Codex #632 P1 #1: a reboot relaunches with a minimal spec (binary + args only).
    /// The namespace isolation captured on the FIRST spawn must persist, or the
    /// relaunched VMM would run outside its namespaces. Before the fix, `spawn` read the
    /// namespace fields straight off the spec, so the minimal relaunch spec dropped them.
    #[test]
    fn namespace_isolation_persists_across_minimal_respawn() {
        let mut be = backend();
        // First spawn (cold boot): full isolation provided by the orchestration.
        be.merge_spec_namespace(&ProcessSpec {
            binary: PathBuf::from("/usr/local/bin/cloud-hypervisor"),
            namespace_id: Some("ns-abc".to_string()),
            user_namespace_path: Some(PathBuf::from("/proc/123/ns/user")),
            net_namespace_path: Some(PathBuf::from("/proc/123/ns/net")),
            mount_redirects: Some(crate::utils::MountRedirects {
                view: PathBuf::from("/vm-disks"),
                pairs: vec![(PathBuf::from("/base"), PathBuf::from("/clone"))],
                keep: vec![],
                keep_files: vec![],
            }),
            ..Default::default()
        });
        // Reboot relaunch: only the binary + args, everything else default/None.
        be.merge_spec_namespace(&ProcessSpec {
            binary: PathBuf::from("/usr/local/bin/cloud-hypervisor"),
            ..Default::default()
        });
        assert_eq!(be.namespace.namespace_id.as_deref(), Some("ns-abc"));
        assert_eq!(
            be.namespace.user_namespace_path.as_deref(),
            Some(Path::new("/proc/123/ns/user"))
        );
        assert_eq!(
            be.namespace.net_namespace_path.as_deref(),
            Some(Path::new("/proc/123/ns/net"))
        );
        assert!(be.namespace.mount_redirects.is_some());
        // The retained vm_id (set in new()) is never clobbered by a merge.
        assert_eq!(be.namespace.vm_id, "vm-test");
    }

    /// A later spec with new values DOES override the retained ones (not just additive).
    #[test]
    fn namespace_merge_overrides_provided_fields() {
        let mut be = backend();
        be.merge_spec_namespace(&ProcessSpec {
            namespace_id: Some("ns-old".to_string()),
            ..Default::default()
        });
        be.merge_spec_namespace(&ProcessSpec {
            namespace_id: Some("ns-new".to_string()),
            ..Default::default()
        });
        assert_eq!(be.namespace.namespace_id.as_deref(), Some("ns-new"));
    }

    /// Codex #632 P1 #3: cmdline translation must rewrite/drop only WHOLE kernel args,
    /// never an embedded substring (the prior `.replace()` could corrupt a value that
    /// merely contained `console=ttyS0` or `pci=off`).
    #[test]
    fn ch_cmdline_rewrites_whole_tokens_only() {
        let out = CloudHypervisorBackend::ch_cmdline(
            "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda rw",
            "fcvm_bootplan=vsock",
        );
        let toks: Vec<&str> = out.split_whitespace().collect();
        assert!(
            toks.contains(&"console=hvc0"),
            "serial console → hvc0: {out}"
        );
        assert!(!toks.contains(&"console=ttyS0"));
        assert!(!toks.contains(&"pci=off"), "pci=off dropped: {out}");
        assert!(toks.contains(&"root=/dev/vda"));
        assert!(
            toks.contains(&"fcvm_bootplan=vsock"),
            "runtime args appended"
        );
        assert!(
            toks.contains(&"fcvm_shutdown=acpi"),
            "shutdown contract appended: {out}"
        );
    }

    #[test]
    fn ch_cmdline_preserves_tokens_that_merely_contain_targets() {
        // Whole-token match only: a token that CONTAINS but does not EQUAL a target is
        // left untouched (the substring `.replace()` would have corrupted these).
        let out = CloudHypervisorBackend::ch_cmdline("console=ttyS0extra fcpci=offx", "");
        let toks: Vec<&str> = out.split_whitespace().collect();
        assert_eq!(
            toks,
            vec![
                "console=ttyS0extra",
                "fcpci=offx",
                "net.ifnames=0",
                "fcvm_shutdown=acpi"
            ]
        );
    }

    #[test]
    fn ch_cmdline_handles_empty_runtime_args() {
        let out = CloudHypervisorBackend::ch_cmdline("console=ttyAMA0 root=/dev/vda", "");
        assert_eq!(
            out,
            "console=hvc0 root=/dev/vda net.ifnames=0 fcvm_shutdown=acpi"
        );
    }

    /// The guest keeps the kernel's `eth0` for its PCI virtio-net NIC, which fc-agent and the
    /// rootfs's networkd unit configure by that name (#806). Given once, it is not repeated.
    #[test]
    fn ch_cmdline_keeps_the_kernel_s_interface_name() {
        let out = CloudHypervisorBackend::ch_cmdline("console=ttyS0 root=/dev/vda", "");
        assert_eq!(out.matches("net.ifnames=0").count(), 1, "{out}");
        let given = CloudHypervisorBackend::ch_cmdline("net.ifnames=0", "");
        assert_eq!(given.matches("net.ifnames=0").count(), 1, "{given}");
    }
}
