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
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use self::api::{
    BalloonConfig, ChClient, ConsoleConfig, CpusConfig, DiskConfig, MemoryConfig, NetConfig,
    PayloadConfig, RngConfig, VmConfig, VsockConfig,
};
use super::{Backend, Capabilities, DriveSpec, Hypervisor, NetIfaceSpec, ProcessSpec};
use crate::utils::{install_namespace_pre_exec, spawn_streaming, NamespaceParams};

/// Guest CID for the host↔guest vsock device (host is always CID 2).
const GUEST_CID: u32 = 3;
/// Total wait for the api-socket to accept + ping (RETRY_COUNT * RETRY_DELAY).
const SOCKET_WAIT_RETRY_COUNT: u32 = 500;
const SOCKET_WAIT_RETRY_DELAY: Duration = Duration::from_millis(10);
const STDERR_TAIL_LINES: usize = 10;
const STDERR_DRAIN_DELAY: Duration = Duration::from_millis(200);

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
    /// The live console-tail task ([`tail_console_to_tracing`]), tracked so a reboot
    /// relaunch (or [`Hypervisor::kill`]) aborts the previous one instead of leaking it.
    /// The relaunch reuses the VM dir and keeps `ch-console.log`, so an orphaned tail
    /// would not self-exit — it must be aborted explicitly.
    console_tail: Option<JoinHandle<()>>,
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
            client: None,
            pending: PendingConfig::default(),
            vsock_path: None,
            console_tail: None,
        }
    }

    fn client(&self) -> Result<&ChClient> {
        self.client.as_ref().context("Cloud Hypervisor not started")
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
    /// console is virtio `hvc0` (not the PL011 `ttyS0`/`ttyAMA0`), it puts virtio
    /// devices on PCI (so `pci=off` must be dropped), and it supports ACPI power-off
    /// natively (so the Firecracker-specific `reboot=t`/`reboot=k` triple-fault hack
    /// must be removed — otherwise `reboot -f` causes CH to *restart* the VM instead
    /// of terminating the VMM process, creating an infinite reboot loop).
    fn ch_cmdline(fc_boot_args: &str, runtime_boot_args: &str) -> String {
        let combined = if runtime_boot_args.is_empty() {
            fc_boot_args.to_string()
        } else {
            format!("{fc_boot_args} {runtime_boot_args}")
        };
        // Token-based (not substring) so we only rewrite/drop whole kernel args, never an
        // embedded value: map the serial console to virtio hvc0, drop pci=off (CH puts
        // virtio devices on PCI), and drop reboot=t/reboot=k (Firecracker-specific
        // triple-fault/keyboard-controller shutdown that CH interprets as a reboot, not
        // a termination; without it the kernel defaults to ACPI which CH handles correctly).
        combined
            .split_whitespace()
            .filter_map(|tok| match tok {
                "pci=off" | "reboot=t" | "reboot=k" => None,
                "console=ttyS0" | "console=ttyAMA0" => Some("console=hvc0"),
                other => Some(other),
            })
            .collect::<Vec<_>>()
            .join(" ")
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
                sleep(STDERR_DRAIN_DELAY).await;
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

        let _ = std::fs::remove_file(&self.api_socket);

        let mut cmd = Command::new(&spec.binary);
        cmd.arg("--api-socket").arg(&self.api_socket);
        if let Some(log_path) = &self.log_path {
            cmd.arg("--log-file").arg(log_path);
            cmd.arg("-v");
        }
        if let Some(extra) = &spec.extra_args {
            for arg in extra.split_whitespace() {
                cmd.arg(arg);
            }
        }

        install_namespace_pre_exec(&mut cmd, &self.namespace)?;

        let stderr_tail = Arc::clone(&self.stderr_tail);
        let child = spawn_streaming(cmd, move |line, is_stderr| {
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
                let important = clean.contains("fc-agent") || clean.contains("[ctr:");
                if important {
                    info!(target: "cloud-hypervisor", "{}", clean);
                } else {
                    debug!(target: "cloud-hypervisor", "{}", clean);
                }
            }
        })
        .context("spawning Cloud Hypervisor process")?;

        self.process = Some(child);
        self.wait_for_api().await?;
        self.client = Some(ChClient::new(self.api_socket.clone()));

        // Tail the guest console (hvc0 → file) into fcvm's tracing logs, mirroring the
        // Firecracker serial-to-stdout capture. Abort any prior tail first: a reboot
        // relaunch reuses the VM dir (keeps ch-console.log), so an orphaned tail would
        // not self-exit and would duplicate console lines. Track the handle so kill()
        // tears it down too.
        if let Some(old) = self.console_tail.take() {
            old.abort();
        }
        if let Some(cpath) = self.console_path() {
            self.console_tail = Some(tokio::spawn(tail_console_to_tracing(cpath)));
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
                    self.process = None;
                    Ok(Some(status))
                }
                None => Ok(None),
            },
            None => bail!("Cloud Hypervisor process not running"),
        }
    }

    async fn wait(&mut self) -> Result<ExitStatus> {
        match self.process.as_mut() {
            Some(p) => {
                let status = p.wait().await.context("waiting for Cloud Hypervisor")?;
                self.process = None;
                Ok(status)
            }
            None => bail!("Cloud Hypervisor process not running"),
        }
    }

    async fn kill(&mut self) -> Result<()> {
        if let Some(tail) = self.console_tail.take() {
            tail.abort();
        }
        if let Some(mut p) = self.process.take() {
            info!(vm_id = %self.vm_id, "killing Cloud Hypervisor process");
            p.kill().await.context("killing Cloud Hypervisor")?;
            let _ = p.wait().await;
        }
        Ok(())
    }

    async fn stream_console(&self, _console_path: &Path) -> Result<mpsc::Receiver<String>> {
        // Cloud Hypervisor's virtio console (hvc0) is captured from the process stdout by
        // spawn_streaming, not via a separate console device file. Return an empty stream.
        let (_tx, rx) = mpsc::channel(1);
        Ok(rx)
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

    async fn add_balloon(&mut self, amount_mib: u32) -> Result<()> {
        self.pending.balloon_mib = Some(amount_mib);
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

/// The default guest CID Cloud Hypervisor uses for the host↔guest vsock device.
pub const fn default_guest_cid() -> u32 {
    GUEST_CID
}

/// Follow the Cloud Hypervisor guest console file and emit each new line to fcvm's
/// tracing logs (the portable equivalent of Firecracker streaming its serial to stdout).
/// fc-agent / container lines go at INFO, kernel/boot noise at DEBUG. The loop exits once
/// the file has been gone for a few seconds (the VM dir was cleaned up), so it never
/// outlives its VM.
async fn tail_console_to_tracing(path: PathBuf) {
    use tokio::io::{AsyncBufReadExt, AsyncSeekExt, BufReader};

    // Wait for Cloud Hypervisor to create the console file at boot.
    for _ in 0..SOCKET_WAIT_RETRY_COUNT {
        if path.exists() {
            break;
        }
        tokio::time::sleep(SOCKET_WAIT_RETRY_DELAY).await;
    }

    let mut pos: u64 = 0;
    let mut missing = 0u32;
    loop {
        match tokio::fs::File::open(&path).await {
            Ok(mut file) => {
                missing = 0;
                if file.seek(std::io::SeekFrom::Start(pos)).await.is_ok() {
                    let mut reader = BufReader::new(file);
                    let mut line = String::new();
                    loop {
                        line.clear();
                        match reader.read_line(&mut line).await {
                            Ok(0) => break, // caught up; poll again after a short sleep
                            Ok(n) => {
                                pos += n as u64;
                                let clean = line.trim_end();
                                if clean.contains("fc-agent") || clean.contains("[ctr:") {
                                    info!(target: "cloud-hypervisor", "{}", clean);
                                } else {
                                    debug!(target: "cloud-hypervisor", "{}", clean);
                                }
                            }
                            Err(_) => break,
                        }
                    }
                }
            }
            Err(_) => {
                // Console file gone (VM dir cleaned up). Stop after a grace window.
                missing += 1;
                if missing > 30 {
                    break;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend() -> CloudHypervisorBackend {
        CloudHypervisorBackend::new(
            "vm-test".to_string(),
            PathBuf::from("/tmp/ch-test.sock"),
            None,
        )
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
            mount_redirects: Some((vec![PathBuf::from("/base")], PathBuf::from("/clone"))),
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
        assert!(
            !toks.contains(&"reboot=k"),
            "reboot=k dropped (FC-specific): {out}"
        );
        assert!(toks.contains(&"root=/dev/vda"));
        assert!(
            toks.contains(&"fcvm_bootplan=vsock"),
            "runtime args appended"
        );
    }

    /// reboot=t (x86 triple-fault) is dropped — it would cause CH to restart the VM
    /// in an infinite loop instead of terminating the VMM process.
    #[test]
    fn ch_cmdline_drops_reboot_t() {
        let out = CloudHypervisorBackend::ch_cmdline(
            "console=ttyS0 reboot=t panic=1 pci=off root=/dev/vda rw",
            "",
        );
        let toks: Vec<&str> = out.split_whitespace().collect();
        assert!(!toks.contains(&"reboot=t"), "reboot=t dropped: {out}");
        assert!(toks.contains(&"panic=1"), "other args preserved: {out}");
    }

    #[test]
    fn ch_cmdline_preserves_tokens_that_merely_contain_targets() {
        // Whole-token match only: a token that CONTAINS but does not EQUAL a target is
        // left untouched (the substring `.replace()` would have corrupted these).
        let out = CloudHypervisorBackend::ch_cmdline("console=ttyS0extra fcpci=offx", "");
        let toks: Vec<&str> = out.split_whitespace().collect();
        assert_eq!(toks, vec!["console=ttyS0extra", "fcpci=offx"]);
    }

    #[test]
    fn ch_cmdline_handles_empty_runtime_args() {
        let out = CloudHypervisorBackend::ch_cmdline("console=ttyAMA0 root=/dev/vda", "");
        assert_eq!(out, "console=hvc0 root=/dev/vda");
    }
}
