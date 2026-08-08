use anyhow::{Context, Result};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tracing::{debug, info, warn};

use super::{types::generate_mac, NetworkConfig, NetworkManager, PortMapping, Protocol};
use crate::paths;
use crate::state::truncate_id;

/// Guest network addressing — pasta provides L2↔L4 translation via bridge
const GUEST_IP: &str = "10.0.2.100";
const GUEST_GATEWAY: &str = "10.0.2.2";
/// Namespace IP on bridge — enables nsenter health checks to route to guest
const NAMESPACE_IP: &str = "10.0.2.1";

/// Guest IPv6 addressing (pasta copies host IPv6 with fd00::/64 fallback)
const GUEST_IPV6: &str = "fd00::100";
const GUEST_IPV6_GATEWAY: &str = "fd00::2";

/// Bridge device name
const BRIDGE_DEVICE: &str = "br0";

/// TAP device name for pasta
const PASTA_DEVICE_NAME: &str = "pasta0";

/// Timeout for waiting for pasta PID file (readiness signal)
const PASTA_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Timeout for waiting for pasta's TAP device to appear in the namespace
const PASTA_DEVICE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Number of recent pasta stderr lines kept for error reporting
const PASTA_STDERR_TAIL_LINES: usize = 20;

/// How long to let the async stderr reader drain the pipe after pasta exits,
/// so the failure error can include what it actually printed.
const PASTA_STDERR_DRAIN_DELAY: std::time::Duration = std::time::Duration::from_millis(200);

/// Heredoc delimiter separating the batched `ip` commands from the shell script.
const IP_BATCH_DELIMITER: &str = "FCVM_IP_BATCH";

/// A list of `ip` commands rendered as a single shell script.
///
/// Configuring one VM's namespace takes ~10 `ip` invocations, and each one is a
/// fork+exec. `ip -batch` reads commands from a stream and applies them all in
/// ONE process, so a whole phase costs `nsenter` + `bash` + `ip` instead of a
/// process per command.
///
/// Semantics are unchanged: `ip -batch` runs the commands in order and, without
/// `-force`, **aborts at the first failure** — nothing after it is applied, just
/// as `set -e` did for the one-command-per-line form. On failure it prints
/// `Command failed -:<line>`, and [`IpBatchScript::describe_failure`] maps that
/// line back to the step's description so an error still names the step that
/// failed.
#[derive(Debug, Clone)]
pub struct IpBatchScript {
    /// (human description, `ip` arguments) — one entry per batch line, in order.
    steps: Vec<(String, String)>,
    script: String,
}

impl IpBatchScript {
    /// Render `steps` as one `ip -batch` invocation, followed by `trailing_shell`
    /// lines (for things `ip` cannot do, e.g. writing a sysctl — those are shell
    /// builtins and cost no extra process).
    fn new(steps: Vec<(String, String)>, trailing_shell: &[&str]) -> Self {
        // One step per physical line, no blanks or comments inside the heredoc:
        // `ip -batch` reports the physical line number, so line N is step N.
        let batch: String = steps
            .iter()
            .map(|(_, args)| format!("{}\n", args))
            .collect();
        let mut script = format!(
            "set -e\nip -batch - <<'{delim}'\n{batch}{delim}\n",
            delim = IP_BATCH_DELIMITER,
            batch = batch,
        );
        for line in trailing_shell {
            script.push_str(line);
            script.push('\n');
        }
        Self { steps, script }
    }

    /// The shell script to run under `bash -c` inside the namespace.
    pub fn script(&self) -> &str {
        &self.script
    }

    /// One-line summary of the steps, for debug logging.
    pub fn summary(&self) -> String {
        self.steps
            .iter()
            .map(|(_, args)| format!("ip {}", args))
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// Turn a failed run's stderr into a message naming the step that failed.
    ///
    /// `ip -batch` prints `Command failed -:<line>` for the aborting command;
    /// anything else (nsenter/bash failures, or an `ip` too old to report a
    /// line) falls back to the raw stderr so no diagnostic is ever swallowed.
    pub fn describe_failure(&self, stderr: &str) -> String {
        let stderr = stderr.trim();
        for token in stderr.split_whitespace() {
            let Some(line_no) = token.strip_prefix("-:") else {
                continue;
            };
            let Ok(line_no) = line_no.trim_end_matches(':').parse::<usize>() else {
                continue;
            };
            if let Some((desc, args)) = line_no.checked_sub(1).and_then(|i| self.steps.get(i)) {
                return format!(
                    "step {}/{} ({}) failed running `ip {}`: {}",
                    line_no,
                    self.steps.len(),
                    desc,
                    args,
                    if stderr.is_empty() {
                        "(no stderr)"
                    } else {
                        stderr
                    }
                );
            }
        }
        if stderr.is_empty() {
            "(no stderr)".to_string()
        } else {
            stderr.to_string()
        }
    }
}

/// Rootless networking using pasta with bridge architecture
///
/// This mode uses user namespaces and pasta (from passt project) for true
/// unprivileged operation. No sudo/root required — everything runs in user
/// namespace via nsenter.
///
/// Architecture (L2 Bridge + L4 translation):
/// ```text
/// Host                    | User Namespace (unshare --user --net)
///                         |
/// pasta  <----------------+-- pasta0 --+
///   (L2↔L4 translation,   |            |
///    splice zero-copy)     |           br0 (L2 bridge)
///                         |            |
///                         |          tap-fc ---> Firecracker VM
///                         |                      (guest: 10.0.2.100)
/// ```
///
/// pasta uses L4 translation for efficient networking without a userspace TCP/IP stack.
/// Outbound traffic goes through pasta's L2 TAP path (userspace processing).
/// Inbound port forwarding uses splice(2) for zero-copy socket-to-socket transfer:
/// pasta binds on the host, splices directly into the namespace, where the kernel
/// routes to the VM via br0 → tap-fc.
///
/// Setup sequence:
/// 1. Spawn holder process: `unshare --user --net -- sleep infinity`
/// 2. Run pre-setup via nsenter: create Firecracker TAP only
/// 3. Start pasta: creates pasta0 TAP in namespace with L2↔L4 translation
/// 4. Run post-setup via nsenter: create bridge, add both TAPs, enable ip_forward
/// 5. Run Firecracker via nsenter: `nsenter -t HOLDER_PID -U -n -- firecracker ...`
/// 6. Health checks via nsenter: `nsenter -t HOLDER_PID -U -n -- curl guest_ip:80`
pub struct PastaNetwork {
    vm_id: String,
    tap_device: String,   // TAP device for Firecracker (tap-fc)
    pasta_device: String, // TAP device created by pasta (pasta0)
    port_mappings: Vec<PortMapping>,

    // Network addressing (IPv4) — guest uses 10.0.2.x via bridge
    guest_ip: String, // Guest VM IP (10.0.2.100)

    // Network addressing (IPv6)
    guest_ipv6: String, // fd00::100

    // State (populated during setup)
    pasta_process: Option<Child>,
    stderr_tail: Arc<Mutex<VecDeque<String>>>, // last few pasta stderr lines, for failure attribution
    pid_file: Option<PathBuf>,
    loopback_ip: Option<String>, // Unique loopback IP for port forwarding (127.x.y.z)
    holder_pid: Option<u32>,     // Namespace PID (set in post_start)
    restore_mode: bool,          // Skip port probe in post_start (VM not loaded yet)
}

impl PastaNetwork {
    pub fn new(vm_id: String, tap_device: String, port_mappings: Vec<PortMapping>) -> Self {
        Self {
            vm_id,
            tap_device,
            pasta_device: PASTA_DEVICE_NAME.to_string(),
            port_mappings,
            guest_ip: GUEST_IP.to_string(),
            guest_ipv6: GUEST_IPV6.to_string(),
            pasta_process: None,
            stderr_tail: Arc::new(Mutex::new(VecDeque::new())),
            pid_file: None,
            loopback_ip: None,
            holder_pid: None,
            restore_mode: false,
        }
    }

    /// Set a unique loopback IP for port forwarding (127.x.y.z)
    ///
    /// Each VM gets a unique loopback IP so multiple VMs can forward the same
    /// port numbers (e.g., all VMs can have -p 8080:80).
    ///
    /// On Linux, the entire 127.0.0.0/8 range routes to loopback without needing
    /// `ip addr add`. We just bind directly to 127.0.0.2:8080, 127.0.0.3:8080, etc.
    /// This is fully rootless!
    pub fn with_loopback_ip(mut self, loopback_ip: String) -> Self {
        self.loopback_ip = Some(loopback_ip);
        self
    }

    /// Skip port forwarding probe in post_start() for snapshot restore.
    ///
    /// During snapshot restore, post_start() runs BEFORE the VM snapshot is loaded
    /// into Firecracker. Probing ports at that point forces pasta to attempt L2
    /// forwarding to a non-existent guest, which can poison pasta's internal
    /// connection tracking and cause subsequent connections to return 0 bytes.
    /// The proper verification happens later via verify_port_forwarding() after
    /// the VM is resumed and fc-agent has sent its gratuitous ARP.
    pub fn with_restore_mode(mut self) -> Self {
        self.restore_mode = true;
        self
    }

    /// Get the loopback IP assigned to this VM for port forwarding
    pub fn loopback_ip(&self) -> Option<&str> {
        self.loopback_ip.as_deref()
    }

    /// Build the holder command for creating the namespace
    ///
    /// Returns command to spawn a holder process that keeps the namespace alive.
    /// The holder runs `sleep infinity` which blocks forever until killed.
    /// Note: We use sleep instead of cat because cat requires stdin management.
    ///
    /// UID/GID mapping is handled by setup_namespace_mappings() in common.rs after
    /// the namespace is created (tries newuidmap first, falls back to single-UID mapping).
    pub fn build_holder_command(&self) -> Vec<String> {
        vec![
            "unshare".to_string(),
            "--user".to_string(),
            "--net".to_string(),
            "--".to_string(),
            "sleep".to_string(),
            "infinity".to_string(),
        ]
    }

    /// Build the pre-pasta setup script to run inside the namespace via nsenter.
    ///
    /// Creates the Firecracker TAP device, brings loopback up, and verifies the
    /// TAP exists — every `ip` command in ONE `ip -batch` process (see
    /// [`IpBatchScript`]). The bridge and pasta0 TAP are set up after pasta
    /// starts (pasta creates its own TAP).
    ///
    /// Run via: nsenter -t HOLDER_PID -U -n -- bash -c '<script()>'
    pub fn build_setup_script(&self) -> IpBatchScript {
        IpBatchScript::new(
            vec![
                (
                    format!("create TAP device {} for Firecracker", self.tap_device),
                    format!("tuntap add {} mode tap", self.tap_device),
                ),
                (
                    format!("bring TAP device {} up", self.tap_device),
                    format!("link set {} up", self.tap_device),
                ),
                (
                    "bring loopback up".to_string(),
                    "link set lo up".to_string(),
                ),
                // Verification is the last batch step rather than a second
                // nsenter+ip process: `ip -batch` already stops at the first
                // failure, so reaching this line means every step above applied.
                (
                    format!("verify TAP device {} exists", self.tap_device),
                    format!("link show {}", self.tap_device),
                ),
            ],
            &[],
        )
    }

    /// Build the post-pasta setup script that creates the bridge after pasta is ready
    ///
    /// Connects pasta's TAP and Firecracker's TAP via an L2 bridge.
    /// Port forwarding: pasta splices inbound loopback connections directly into the
    /// namespace, where they route via br0 → tap-fc → VM. Outbound traffic goes
    /// through pasta's L2 translation: tap-fc → br0 → pasta0 → pasta → host.
    ///
    /// The caller (post_start) waits for pasta's TAP device to exist via
    /// wait_for_pasta_device() before running this script.
    ///
    /// All six `ip` commands run in ONE `ip -batch` process; enabling IP
    /// forwarding is a shell redirect (no extra process).
    pub fn build_bridge_script(&self) -> IpBatchScript {
        let bridge = BRIDGE_DEVICE;
        IpBatchScript::new(
            vec![
                (
                    format!(
                        "bring {} up (pasta creates it but leaves it down without --config-net)",
                        self.pasta_device
                    ),
                    format!("link set {} up", self.pasta_device),
                ),
                (
                    format!("create L2 bridge {}", bridge),
                    format!("link add {} type bridge", bridge),
                ),
                (
                    format!("bring bridge {} up", bridge),
                    format!("link set {} up", bridge),
                ),
                (
                    format!("add pasta TAP {} to bridge {}", self.pasta_device, bridge),
                    format!("link set {} master {}", self.pasta_device, bridge),
                ),
                (
                    format!(
                        "add Firecracker TAP {} to bridge {}",
                        self.tap_device, bridge
                    ),
                    format!("link set {} master {}", self.tap_device, bridge),
                ),
                (
                    format!(
                        "add health-check IP {}/24 to bridge {}",
                        NAMESPACE_IP, bridge
                    ),
                    format!("addr add {}/24 dev {}", NAMESPACE_IP, bridge),
                ),
            ],
            // Enable IP forwarding — a bash redirect, not another process.
            &["echo 1 > /proc/sys/net/ipv4/ip_forward"],
        )
    }

    /// Build the nsenter prefix command for running processes in the namespace
    ///
    /// Returns: ["nsenter", "-t", "PID", "-U", "-n", "--preserve-credentials", "--"]
    /// The --preserve-credentials flag keeps UID/GID/groups (including kvm) for KVM access.
    /// Append command and args after this.
    pub fn build_nsenter_prefix(&self, holder_pid: u32) -> Vec<String> {
        vec![
            "nsenter".to_string(),
            "-t".to_string(),
            holder_pid.to_string(),
            "-U".to_string(),
            "-n".to_string(),
            "--preserve-credentials".to_string(),
            "--".to_string(),
        ]
    }

    /// Get a human-readable representation of the rootless networking flow
    pub fn rootless_flow_string(&self) -> String {
        "holder(unshare --user --net) + nsenter for setup/firecracker".to_string()
    }

    /// Detect host's global IPv6 address for pasta outbound traffic.
    ///
    /// Memoised for the process lifetime: `setup()` and `start_pasta()` both need
    /// it, so without this every VM launch pays two `ip -6 addr show` execs for
    /// an answer that cannot change during one launch.
    fn detect_host_ipv6() -> Option<String> {
        static HOST_IPV6: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
        HOST_IPV6.get_or_init(Self::probe_host_ipv6).clone()
    }

    fn probe_host_ipv6() -> Option<String> {
        let output = std::process::Command::new("ip")
            .args(["-6", "addr", "show", "scope", "global"])
            .output()
            .ok()?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            let line = line.trim();
            if line.starts_with("inet6 ") {
                if let Some(addr_part) = line.strip_prefix("inet6 ") {
                    if let Some(addr) = addr_part.split('/').next() {
                        // Skip link-local (fe80::) and ULA (fd00::)
                        if !addr.starts_with("fe80:") && !addr.starts_with("fd") {
                            return Some(addr.to_string());
                        }
                    }
                }
            }
        }
        None
    }

    /// Detect HTTP proxy from host environment
    ///
    /// On IPv6-only hosts, traffic must go through a proxy.
    /// Returns the proxy URL with IPv6 address resolved from hostname.
    fn detect_http_proxy() -> Option<String> {
        let proxy_url = std::env::var("HTTP_PROXY")
            .or_else(|_| std::env::var("http_proxy"))
            .or_else(|_| std::env::var("HTTPS_PROXY"))
            .or_else(|_| std::env::var("https_proxy"))
            .ok()?;

        if let Some(rest) = proxy_url.strip_prefix("http://") {
            let host_port = rest.trim_end_matches('/');

            if host_port.starts_with('[') {
                return Some(proxy_url);
            }

            if let Some((host, port)) = host_port.rsplit_once(':') {
                if let Ok(output) = std::process::Command::new("getent")
                    .args(["hosts", host])
                    .output()
                {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    if let Some(ipv6) = stdout.split_whitespace().next() {
                        if ipv6.contains(':') {
                            return Some(format!("http://[{}]:{}", ipv6, port));
                        }
                    }
                }
                return Some(proxy_url);
            }
        }

        Some(proxy_url)
    }

    /// Start pasta process attached to the namespace
    ///
    /// pasta creates its own TAP device (pasta0) in the namespace and provides
    /// L2↔L4 translation to the host. Uses PID file for readiness signaling.
    pub async fn start_pasta(&mut self, namespace_pid: u32) -> Result<()> {
        let pid_file = paths::data_dir().join(format!("pasta-{}.pid", truncate_id(&self.vm_id, 8)));

        // Register the inotify watch BEFORE the stale-file cleanup and spawn:
        // pasta's PID-file write after this point always produces an event, so
        // readiness can never be missed (zero-race: watch → check → wait).
        // Init failure (e.g. fs.inotify.max_user_instances exhausted by many
        // concurrent clone starts as one user) must not fail start_pasta — the
        // old poll never had that failure mode. Degrade to the 250ms safety
        // tick below, same as the vm.rs API-socket watch.
        let mut pid_file_watch = match crate::utils::DirWatch::new(&paths::data_dir()) {
            Ok(w) => Some(w),
            Err(e) => {
                warn!(
                    error = %e,
                    "inotify unavailable for pasta PID-file wait; degrading to 250ms polling"
                );
                None
            }
        };

        if pid_file.exists() {
            tokio::fs::remove_file(&pid_file).await?;
        }

        let host_ipv6 = Self::detect_host_ipv6();

        info!(
            namespace_pid = namespace_pid,
            pasta_tap = %self.pasta_device,
            pid_file = %pid_file.display(),
            host_ipv6 = ?host_ipv6,
            port_mappings = self.port_mappings.len(),
            "starting pasta for rootless networking"
        );

        // Resolve the pasta binary through the pinned-build machinery: with a
        // [pasta] config section the content-addressed patched build is
        // required (a distro pasta would reintroduce the addr_seen inbound
        // poisoning, #661); without one, PATH is used as before.
        let (config, _, _) =
            crate::setup::rootfs::load_config(None).context("loading config for pasta")?;
        let pasta_bin = crate::setup::get_pasta_for_config(config.pasta.as_ref())?;
        info!(pasta_bin = %pasta_bin.display(), "resolved pasta binary");

        let mut cmd = Command::new(&pasta_bin);
        cmd.arg("--foreground")
            .arg("--quiet")
            .arg("-P")
            .arg(&pid_file);

        // When running as root (e.g., sudo in tests), pasta drops to nobody by
        // default and then can't access the user namespace. Tell it to stay as root.
        if nix::unistd::geteuid().is_root() {
            cmd.arg("--runas").arg("0:0");
        }

        // Don't use --config-net: it sets an IP on pasta0's kernel interface, which
        // conflicts with the bridge (kernel responds to ARP for that IP via bridge's
        // weak host model, stealing traffic from pasta's userspace L2 handler).
        // Instead, pasta creates the TAP but we bring it up in build_bridge_script().
        //
        // -a must be the VM's actual IP (GUEST_IP), not the gateway. pasta uses -a
        // as the "guest address" and ignores ARP requests for it (don't resolve self).
        // If -a == gateway, pasta ignores ARP for the gateway and the VM can't route.
        cmd.arg("--ns-ifname")
            .arg(&self.pasta_device)
            .arg("-a")
            .arg(GUEST_IP) // VM's actual IP — pasta ignores ARP for this address
            .arg("-n")
            .arg("255.255.255.0")
            .arg("-g")
            .arg(GUEST_GATEWAY) // Gateway — pasta responds to ARP for this
            .arg("--no-dhcp");

        // If host has global IPv6, configure pasta for IPv6 outbound
        if let Some(ref ipv6) = host_ipv6 {
            // Add IPv6 guest address and gateway so pasta handles IPv6 L2↔L4 translation.
            // -a/-g can each be specified twice (once IPv4, once IPv6).
            cmd.arg("-a")
                .arg(GUEST_IPV6) // Guest IPv6 address — pasta ignores NDP for this
                .arg("-g")
                .arg(GUEST_IPV6_GATEWAY) // IPv6 gateway — pasta responds to NDP for this
                .arg("-o")
                .arg(ipv6); // Outbound source address for IPv6

            // Keep NDP enabled: the guest needs NDP Neighbor Solicitation/Advertisement
            // to resolve the IPv6 gateway's MAC address (like ARP for IPv4).
            // Disable only RA (router advertisements) and DHCPv6 — we configure the
            // guest's IPv6 address statically via kernel cmdline, not SLAAC.
            cmd.arg("--no-ra").arg("--no-dhcpv6");
        } else {
            // No host IPv6 — disable IPv6 entirely
            cmd.arg("--ipv4-only")
                // NDP/RA/DHCPv6 are moot with --ipv4-only, but be explicit
                .arg("--no-ndp")
                .arg("--no-dhcpv6")
                .arg("--no-ra");
        }

        // Port forwarding: pasta binds on host, L2 frames go through bridge to VM
        if self.port_mappings.is_empty() {
            cmd.arg("-t").arg("none").arg("-u").arg("none");
        } else {
            let mut tcp_specs = Vec::new();
            let mut udp_specs = Vec::new();

            for mapping in &self.port_mappings {
                let bind_addr = match &mapping.host_ip {
                    Some(ip) => ip.as_str(),
                    None => self.loopback_ip.as_deref().unwrap_or("127.0.0.1"),
                };

                // pasta spec: "bind_addr/host_port:guest_port"
                let spec = format!("{}/{}:{}", bind_addr, mapping.host_port, mapping.guest_port);

                match mapping.proto {
                    Protocol::Tcp => tcp_specs.push(spec),
                    Protocol::Udp => udp_specs.push(spec),
                }

                info!(
                    proto = ?mapping.proto,
                    host = %format!("{}:{}", bind_addr, mapping.host_port),
                    guest = %format!("{}:{}", self.guest_ip, mapping.guest_port),
                    "adding port forward"
                );
            }

            if tcp_specs.is_empty() {
                cmd.arg("-t").arg("none");
            } else {
                for spec in &tcp_specs {
                    cmd.arg("-t").arg(spec);
                }
            }
            if udp_specs.is_empty() {
                cmd.arg("-u").arg("none");
            } else {
                for spec in &udp_specs {
                    cmd.arg("-u").arg(spec);
                }
            }
        }

        // Disable host→namespace port forwarding (reverse direction).
        // These don't affect outbound traffic — pasta's L2↔L4 translation handles
        // that independently. Matches Podman's invocation pattern.
        cmd.arg("-T").arg("none").arg("-U").arg("none");

        // Attach to the holder's namespace
        cmd.arg(namespace_pid.to_string());

        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        // Kill pasta if fcvm dies, including from SIGKILL — the same per-hop guarantee
        // `install_namespace_pre_exec` gives the VMM and `spawn_namespace_holder` gives the
        // holder, and the one hop that did not have it.
        //
        // This is hardening, not a fix for an observed leak: pasta joins the holder BY PID,
        // and passt terminates itself when the PID whose namespaces it joined exits, so
        // today pasta already dies whenever the holder does. Measured both ways on this
        // branch — with the pre_exec removed, pasta still exited on fcvm SIGKILL (via the
        // holder), and it still exited even with the holder's netns pinned open by an
        // external fd, confirming the trigger is passt's PID watch rather than namespace
        // teardown.
        //
        // Arming pdeathsig directly is still worth its ten lines. That chain runs through
        // TWO things fcvm does not own: the holder's own pdeathsig, and an undocumented
        // passt behaviour (`--no-netns-quit` documents only the filesystem-bound case) in a
        // binary this repo pins to a patched fork and periodically rebases. AGENTS.md is
        // explicit that teardown is per-hop and that one unprotected hop orphans everything
        // below it; this makes pasta's death depend on nothing but the kernel.
        //
        // Must be the LAST pre_exec (there is only this one, but keep the invariant
        // explicit): a credential change zeroes `task->pdeath_signal`, so anything that
        // switches namespaces or identity first would silently drop it. pasta's own
        // `--runas 0:0` under sudo is a no-op re-assertion of root, and its namespace entry
        // happens in a forked child, so the signal set here survives into steady state.
        //
        // The `getppid` re-check closes the fork/exec window: a parent that dies after
        // fork() but before the prctl above would leave the signal unarmed and pasta
        // orphaned — the very outcome this is here to prevent. If we have already been
        // reparented, fail the exec so no pasta exists to leak.
        //
        // SAFETY: pre_exec runs between fork() and exec(). `prctl` and `getppid` are both
        // async-signal-safe, and the closure allocates nothing (`fcvm_pid` is copied in).
        let fcvm_pid = std::process::id() as libc::pid_t;
        unsafe {
            cmd.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != fcvm_pid {
                    return Err(std::io::Error::other(
                        "parent died before PR_SET_PDEATHSIG was armed",
                    ));
                }
                Ok(())
            });
        }

        debug!(cmd = ?cmd, "pasta command");
        let mut child = cmd.spawn().context("failed to spawn pasta")?;

        // Stream pasta's stderr: log every line and keep a tail so error paths
        // can show what pasta actually printed. Without this, pasta's output is
        // silently discarded and a dead pasta only surfaces later as an
        // unrelated bridge setup failure.
        if let Some(stderr) = child.stderr.take() {
            let stderr_tail = Arc::clone(&self.stderr_tail);
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    warn!(target: "pasta", "{}", line);
                    if let Ok(mut tail) = stderr_tail.lock() {
                        if tail.len() >= PASTA_STDERR_TAIL_LINES {
                            tail.pop_front();
                        }
                        tail.push_back(line);
                    }
                }
            });
        }

        // Wait for the PID file to appear (pasta writes it once ready).
        // Event-driven: the inotify watch registered before spawn wakes us the
        // instant the file lands (the old 50ms poll noticed a +2.3ms file at
        // +52.8ms on every rootless clone). pasta death interrupts the wait via
        // child.wait(); the deadline still bounds everything, and a coarse
        // safety tick re-checks the file so even a lost/overflowed inotify
        // event can only add latency, never hang.
        let deadline = tokio::time::Instant::now() + PASTA_READY_TIMEOUT;
        loop {
            if pid_file.exists() {
                info!("pasta ready (PID file created)");
                break;
            }

            tokio::select! {
                status = child.wait() => {
                    // pasta died during startup. Give the stderr reader a moment
                    // to drain the pipe so the error includes what pasta printed.
                    tokio::time::sleep(PASTA_STDERR_DRAIN_DELAY).await;
                    match status {
                        Ok(status) => anyhow::bail!(
                            "pasta exited before becoming ready (status: {}){}",
                            status,
                            self.stderr_tail_message()
                        ),
                        Err(e) => anyhow::bail!("failed to check pasta status: {}", e),
                    }
                }
                event = crate::utils::DirWatch::next_event_opt(&mut pid_file_watch) => {
                    // Filesystem activity in the data dir — loop re-checks the
                    // PID file. A watch error degrades to the safety tick.
                    if event.is_err() {
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    let _ = child.kill().await;
                    anyhow::bail!(
                        "pasta did not become ready within {:?}{}",
                        PASTA_READY_TIMEOUT,
                        self.stderr_tail_message()
                    );
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => {
                    // Safety tick: re-check the condition even without events.
                }
            }
        }

        self.pasta_process = Some(child);
        self.pid_file = Some(pid_file);

        Ok(())
    }

    /// Render the captured pasta stderr tail for error messages.
    fn stderr_tail_message(&self) -> String {
        let lines: Vec<String> = self
            .stderr_tail
            .lock()
            .map(|tail| tail.iter().cloned().collect())
            .unwrap_or_default();
        if lines.is_empty() {
            "; no stderr output captured from pasta".to_string()
        } else {
            format!("; last pasta stderr output:\n  {}", lines.join("\n  "))
        }
    }

    /// Wait for pasta's TAP device to appear in the namespace, supervising pasta itself.
    ///
    /// pasta writes its PID file before the device is visible in the namespace,
    /// and under load that window can stretch out — or pasta can die right after
    /// startup, in which case the device never appears at all. Polling here
    /// (instead of inside the bridge script) lets every iteration also check the
    /// pasta child, so a dead pasta fails fast with its own exit status and
    /// stderr instead of a generic "Cannot find device" from the bridge setup.
    async fn wait_for_pasta_device(&mut self, holder_pid: u32) -> Result<()> {
        let deadline = std::time::Instant::now() + PASTA_DEVICE_TIMEOUT;
        let nsenter_prefix = self.build_nsenter_prefix(holder_pid);

        // Fine-grained backoff, not a fixed 100ms: pasta creates the TAP within
        // a few ms of writing its PID file, and now that the PID file is
        // noticed event-driven (+2ms instead of +52ms) this probe regularly
        // arrives BEFORE the device exists — a fixed 100ms retry put a +100ms
        // mode on 4/10 benched clones. Each probe is a full nsenter+ip exec
        // (~1-3ms), so short gaps just keep probing continuously through the
        // handful of ms until the device appears; the deadline still bounds it.
        let mut retry_delay = std::time::Duration::from_millis(1);

        loop {
            let output = Command::new(&nsenter_prefix[0])
                .args(&nsenter_prefix[1..])
                .args(["ip", "link", "show", &self.pasta_device])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .output()
                .await
                .context("checking for pasta TAP device via nsenter")?;

            if output.status.success() {
                debug!(device = %self.pasta_device, "pasta TAP device present in namespace");
                return Ok(());
            }

            // Device not visible yet — if pasta has exited, attribute the failure
            // to pasta instead of letting the bridge setup fail later.
            let pasta_exit = match self.pasta_process.as_mut() {
                Some(process) => process
                    .try_wait()
                    .context("checking pasta process status")?,
                None => None,
            };
            if let Some(status) = pasta_exit {
                self.pasta_process = None;
                // Give the stderr reader a moment to drain the pipe so the
                // error includes what pasta actually printed.
                tokio::time::sleep(PASTA_STDERR_DRAIN_DELAY).await;
                anyhow::bail!(
                    "pasta exited (status: {}) before its TAP device {} appeared in the namespace{}",
                    status,
                    self.pasta_device,
                    self.stderr_tail_message()
                );
            }

            if std::time::Instant::now() > deadline {
                anyhow::bail!(
                    "pasta is still running but its TAP device {} did not appear in the namespace within {:?}{}",
                    self.pasta_device,
                    PASTA_DEVICE_TIMEOUT,
                    self.stderr_tail_message()
                );
            }

            tokio::time::sleep(retry_delay).await;
            retry_delay = (retry_delay * 2).min(std::time::Duration::from_millis(50));
        }
    }

    /// Get guest IP address for kernel boot args
    pub fn guest_ip(&self) -> &str {
        &self.guest_ip
    }

    /// Get gateway IP for guest (pasta gateway)
    pub fn gateway_ip(&self) -> &str {
        GUEST_GATEWAY
    }

    /// Wait for pasta port forwarding to be ready by probing each mapped port.
    ///
    /// Pasta binds ports asynchronously after startup. The PID file just means
    /// the process is running, not that ports are listening. Without this check,
    /// the health monitor may declare the VM "healthy" (via nsenter/bridge) before
    /// port forwarding actually works.
    async fn wait_for_port_forwarding(&self) -> Result<()> {
        use tokio::net::TcpStream;

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let loopback = self.loopback_ip.as_deref().unwrap_or("127.0.0.1");

        for mapping in &self.port_mappings {
            if mapping.proto != Protocol::Tcp {
                continue;
            }

            let bind_addr = match &mapping.host_ip {
                Some(ip) => ip.as_str(),
                None => loopback,
            };
            let addr = format!("{}:{}", bind_addr, mapping.host_port);

            loop {
                match TcpStream::connect(&addr).await {
                    Ok(_) => {
                        debug!(addr = %addr, "port forward ready");
                        break;
                    }
                    Err(_) => {
                        if std::time::Instant::now() > deadline {
                            anyhow::bail!("pasta port forward not ready within 5s: {}", addr);
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                }
            }
        }

        Ok(())
    }
}

#[async_trait::async_trait]
impl NetworkManager for PastaNetwork {
    async fn setup(&mut self) -> Result<NetworkConfig> {
        info!(vm_id = %self.vm_id, "setting up rootless networking with pasta (bridge mode)");

        info!(
            guest_ip = %self.guest_ip,
            gateway = %GUEST_GATEWAY,
            loopback_ip = ?self.loopback_ip,
            "network configuration (pasta bridge mode, nsenter health checks)"
        );

        let guest_mac = generate_mac();

        // Check if host has IPv6 — pasta handles it natively
        let (guest_ipv6, host_ipv6) = if Self::detect_host_ipv6().is_some() {
            (
                Some(self.guest_ipv6.clone()),
                Some(GUEST_IPV6_GATEWAY.to_string()),
            )
        } else {
            (None, None)
        };

        let http_proxy = Self::detect_http_proxy();
        if let Some(ref proxy) = http_proxy {
            info!(proxy = %proxy, "detected HTTP proxy for IPv6-only network");
        }

        Ok(NetworkConfig {
            tap_device: self.tap_device.clone(),
            guest_mac,
            guest_ip: Some(format!("{}/24", self.guest_ip)),
            host_ip: Some(GUEST_GATEWAY.to_string()),
            host_veth: None,
            loopback_ip: self.loopback_ip.clone(),
            // Don't use pasta's DNS forwarder (10.0.2.3) — it's unreachable from the VM
            // through the bridge. Instead, pass host DNS servers directly; the guest
            // reaches them via pasta's L4 translation (same path as all other traffic).
            dns_server: None,
            guest_ipv6,
            host_ipv6,
            dns_search: None,
            http_proxy,
            namespace_name: None,
        })
    }

    async fn post_start(&mut self, holder_pid: u32) -> Result<()> {
        self.holder_pid = Some(holder_pid);

        info!(
            holder_pid = holder_pid,
            "starting pasta for rootless networking"
        );

        // Phases 1+2: start pasta and wait for its TAP device, with a bounded
        // retry on a transient startup failure.
        //
        // pasta has a netlink startup race: it subscribes to route/neighbour
        // notifications and then issues request/response netlink calls during
        // setup; a notification (sequence 0) arriving mid-sequence makes
        // nl_status() die() with "netlink: Unexpected sequence number". Upstream
        // d00255bd fixed the neighbour-sync path, but the race still recurs under
        // heavy parallelism (many pastas starting while veth/tap/bridge churn
        // generates netlink traffic), so pasta exits before its TAP appears.
        // It is transient — a fresh start almost always succeeds — so retry a few
        // times rather than failing the whole VM. (The remaining race is reported
        // upstream; this keeps us resilient until it lands and the pin is bumped.)
        const PASTA_START_ATTEMPTS: u32 = 4;
        let mut last_err = None;
        for attempt in 1..=PASTA_START_ATTEMPTS {
            let result = match self.start_pasta(holder_pid).await {
                Ok(()) => self.wait_for_pasta_device(holder_pid).await,
                Err(e) => Err(e),
            };
            match result {
                Ok(()) => {
                    last_err = None;
                    break;
                }
                Err(e) => {
                    warn!(
                        attempt,
                        max = PASTA_START_ATTEMPTS,
                        error = %e,
                        "pasta startup failed (likely the transient netlink race), retrying"
                    );
                    // Reap the dead pasta (if start_pasta got far enough to store it)
                    // so the next attempt starts clean; start_pasta also removes a
                    // stale PID file at its start.
                    if let Some(mut p) = self.pasta_process.take() {
                        let _ = p.kill().await;
                    }
                    last_err = Some(e);
                    if attempt < PASTA_START_ATTEMPTS {
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    }
                }
            }
        }
        if let Some(e) = last_err {
            return Err(e.context(format!(
                "pasta failed to start after {PASTA_START_ATTEMPTS} attempts"
            )));
        }

        // Phase 3: Create bridge connecting pasta0 and Firecracker's TAP
        let bridge_script = self.build_bridge_script();
        let nsenter_prefix = self.build_nsenter_prefix(holder_pid);

        debug!(
            holder_pid = holder_pid,
            script = %bridge_script.summary(),
            "running bridge setup script"
        );

        let output = Command::new(&nsenter_prefix[0])
            .args(&nsenter_prefix[1..])
            .arg("bash")
            .arg("-c")
            .arg(bridge_script.script())
            .output()
            .await
            .context("running bridge setup via nsenter")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "bridge setup failed: {}",
                bridge_script.describe_failure(&stderr)
            );
        }

        // Phase 4: Verify port forwarding is actually working
        // The PID file only means pasta spawned, not that ports are bound.
        // Health checks use nsenter (bridge path), so without this check
        // "healthy" doesn't mean port forwarding works.
        //
        // Skip in restore mode: during snapshot restore, post_start() runs BEFORE
        // the VM snapshot is loaded. Probing ports now forces pasta to attempt L2
        // forwarding to a non-existent guest, poisoning its connection state and
        // causing subsequent connections to return 0 bytes. The port check happens
        // later via verify_port_forwarding() after the VM is actually running.
        if !self.restore_mode && !self.port_mappings.is_empty() {
            self.wait_for_port_forwarding().await?;
        }

        info!(holder_pid = holder_pid, "pasta + bridge setup complete");
        Ok(())
    }

    fn start_kill_processes(&mut self) {
        if let Some(process) = self.pasta_process.as_mut() {
            if let Err(e) = process.start_kill() {
                warn!(vm_id = %self.vm_id, error = %e, "failed to signal pasta");
            }
        }
    }

    async fn cleanup(&mut self) -> Result<()> {
        info!(vm_id = %self.vm_id, "cleaning up pasta resources");

        // `kill()` is start_kill + wait, so this stays correct whether or not
        // `start_kill_processes` already signalled pasta (re-signalling a not-yet-reaped
        // process is a no-op); when it did, the wait below has nothing left to wait for.
        if let Some(mut process) = self.pasta_process.take() {
            if let Err(e) = process.kill().await {
                warn!("failed to kill pasta: {}", e);
            }
        }

        if let Some(ref pid_file) = self.pid_file {
            if pid_file.exists() {
                if let Err(e) = tokio::fs::remove_file(pid_file).await {
                    warn!("failed to remove pasta PID file: {}", e);
                }
            }
        }

        info!(vm_id = %self.vm_id, "pasta cleanup complete");
        Ok(())
    }

    fn tap_device(&self) -> &str {
        &self.tap_device
    }

    /// Verify pasta's L2 forwarding path is ready after snapshot restore.
    ///
    /// After snapshot restore, pasta needs the guest's MAC address to forward
    /// L2 frames. We actively ping the guest from the namespace to trigger a
    /// normal ARP exchange. With arp_accept=0 (Linux default), the guest's
    /// gratuitous arping does NOT create neighbor entries — only updates
    /// existing ones. The active ping forces the namespace kernel to send an
    /// ARP request that the guest replies to, creating a REACHABLE entry.
    ///
    /// Once ARP is resolved, we probe each forwarded port to confirm pasta's
    /// loopback port forwarding is end-to-end functional.
    async fn verify_port_forwarding(&self) -> Result<()> {
        if self.port_mappings.is_empty() {
            return Ok(());
        }

        let holder_pid = match self.holder_pid {
            Some(pid) => pid,
            None => return Ok(()),
        };

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let nsenter_prefix = self.build_nsenter_prefix(holder_pid);

        // Ping the guest from inside the namespace to trigger ARP resolution.
        // A successful ping proves ARP resolved AND the guest is reachable.
        // Use 200ms timeout for ~16 retries within the 5s deadline.
        loop {
            let output = Command::new(&nsenter_prefix[0])
                .args(&nsenter_prefix[1..])
                .args(["ping", "-c", "1", "-W", "0.2", GUEST_IP])
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .output()
                .await
                .context("running ping via nsenter in namespace")?;

            if output.status.success() {
                info!(
                    guest_ip = GUEST_IP,
                    "guest reachable via ping, ARP resolved"
                );
                self.wait_for_port_forwarding().await?;
                return Ok(());
            }

            if std::time::Instant::now() > deadline {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let stderr = stderr.trim();
                anyhow::bail!(
                    "ARP for guest {} not resolved within 5s on {}: ping stderr: {}",
                    GUEST_IP,
                    BRIDGE_DEVICE,
                    if stderr.is_empty() { "(empty)" } else { stderr }
                );
            }

            debug!(guest_ip = GUEST_IP, "ping to guest failed, retrying");
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_network_creation() {
        let net = PastaNetwork::new("vm-test123".to_string(), "tap0".to_string(), vec![]);

        assert_eq!(net.tap_device, "tap0");
        assert_eq!(net.pasta_device, "pasta0");
        assert_eq!(net.guest_ip, "10.0.2.100");
        assert_eq!(net.gateway_ip(), "10.0.2.2");
    }

    /// Batch lines are exactly the heredoc body, in order, one per line.
    fn batch_lines(script: &str) -> Vec<&str> {
        let open = format!("<<'{}'\n", IP_BATCH_DELIMITER);
        let body = script
            .split_once(&open)
            .expect("script must open a batch heredoc")
            .1;
        let body = body
            .split_once(&format!("\n{}\n", IP_BATCH_DELIMITER))
            .expect("script must close the batch heredoc")
            .0;
        body.lines().collect()
    }

    #[test]
    fn setup_script_batches_every_ip_command_into_one_process() {
        let net = PastaNetwork::new("vm-test123".to_string(), "tap-fc".to_string(), vec![]);
        let script = net.build_setup_script();

        // Exactly one `ip` process for the whole phase: the only shell line that
        // invokes `ip` is the batch itself.
        let ip_lines: Vec<&str> = script
            .script()
            .lines()
            .filter(|l| l.trim_start().starts_with("ip "))
            .collect();
        assert_eq!(
            ip_lines,
            vec![format!("ip -batch - <<'{}'", IP_BATCH_DELIMITER)],
            "no per-command `ip` invocations should remain outside the batch"
        );

        assert_eq!(
            batch_lines(script.script()),
            vec![
                "tuntap add tap-fc mode tap",
                "link set tap-fc up",
                "link set lo up",
                "link show tap-fc",
            ],
            "TAP verification must be the last batch step, not a second exec"
        );
    }

    #[test]
    fn bridge_script_batches_every_ip_command_and_keeps_ip_forward() {
        let net = PastaNetwork::new("vm-test123".to_string(), "tap-fc".to_string(), vec![]);
        let script = net.build_bridge_script();

        assert_eq!(script.script().matches("ip -batch -").count(), 1);
        assert_eq!(
            batch_lines(script.script()),
            vec![
                "link set pasta0 up",
                "link add br0 type bridge",
                "link set br0 up",
                "link set pasta0 master br0",
                "link set tap-fc master br0",
                "addr add 10.0.2.1/24 dev br0",
            ]
        );
        assert!(
            script
                .script()
                .contains("echo 1 > /proc/sys/net/ipv4/ip_forward"),
            "ip_forward must still be enabled (as a shell redirect, not a process)"
        );
        assert!(script.script().starts_with("set -e\n"));
    }

    #[test]
    fn batch_failure_names_the_failing_step() {
        let net = PastaNetwork::new("vm-test123".to_string(), "tap-fc".to_string(), vec![]);
        let script = net.build_bridge_script();

        // `ip -batch` aborts at the first failing line and reports `-:<line>`.
        let msg = script.describe_failure("RTNETLINK answers: File exists\nCommand failed -:2\n");
        assert!(msg.contains("step 2/6"), "missing step index: {msg}");
        assert!(
            msg.contains("create L2 bridge br0"),
            "missing step name: {msg}"
        );
        assert!(
            msg.contains("ip link add br0 type bridge"),
            "missing command: {msg}"
        );
        assert!(
            msg.contains("RTNETLINK answers: File exists"),
            "lost stderr: {msg}"
        );
    }

    #[test]
    fn batch_failure_falls_back_to_raw_stderr() {
        let net = PastaNetwork::new("vm-test123".to_string(), "tap-fc".to_string(), vec![]);
        let script = net.build_setup_script();

        // nsenter-level failure: no `-:<line>` marker, so nothing may be swallowed.
        let msg = script.describe_failure("nsenter: cannot open /proc/123/ns/net: No such process");
        assert_eq!(
            msg,
            "nsenter: cannot open /proc/123/ns/net: No such process"
        );

        // Out-of-range line numbers must not panic or invent a step.
        assert_eq!(
            script.describe_failure("Command failed -:99"),
            "Command failed -:99"
        );
        assert_eq!(script.describe_failure("   "), "(no stderr)");
    }

    /// The batch really does run under `bash -c` and really does abort at the
    /// first failure — verified against the host's actual `ip` binary, in the
    /// current namespace, using read-only `link show` commands.
    #[test]
    fn ip_batch_script_runs_and_aborts_on_first_failure() {
        let ok = IpBatchScript::new(
            vec![
                ("show loopback".to_string(), "link show lo".to_string()),
                (
                    "show loopback again".to_string(),
                    "link show lo".to_string(),
                ),
            ],
            &["echo TRAILING_RAN"],
        );
        let out = std::process::Command::new("bash")
            .arg("-c")
            .arg(ok.script())
            .output()
            .expect("running batch script");
        assert!(
            out.status.success(),
            "batch failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(String::from_utf8_lossy(&out.stdout).contains("TRAILING_RAN"));

        let bad = IpBatchScript::new(
            vec![
                ("show loopback".to_string(), "link show lo".to_string()),
                (
                    "show a device that cannot exist".to_string(),
                    "link show fcvm-no-such-dev".to_string(),
                ),
                ("never reached".to_string(), "link show lo".to_string()),
            ],
            &["echo TRAILING_RAN"],
        );
        let out = std::process::Command::new("bash")
            .arg("-c")
            .arg(bad.script())
            .output()
            .expect("running batch script");
        assert!(!out.status.success(), "batch should have failed");
        let stderr = String::from_utf8_lossy(&out.stderr);
        let msg = bad.describe_failure(&stderr);
        assert!(
            msg.contains("step 2/3") && msg.contains("show a device that cannot exist"),
            "failure not attributed to step 2: stderr={stderr:?} msg={msg}"
        );
        assert!(
            !String::from_utf8_lossy(&out.stdout).contains("TRAILING_RAN"),
            "set -e must stop the script when the batch aborts"
        );
    }
}
