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

    /// Build the pre-pasta setup script to run inside the namespace via nsenter
    ///
    /// Creates only the Firecracker TAP device. The bridge and pasta0 TAP
    /// are set up after pasta starts (pasta creates its own TAP).
    /// Run via: nsenter -t HOLDER_PID -U -n -- bash -c '<this script>'
    pub fn build_setup_script(&self) -> String {
        format!(
            r#"
set -e

# Create TAP device for Firecracker (pasta creates its own TAP separately)
ip tuntap add {fc_tap} mode tap
ip link set {fc_tap} up

# Set up loopback
ip link set lo up
"#,
            fc_tap = self.tap_device,
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
    pub fn build_bridge_script(&self) -> String {
        let script = format!(
            r#"
set -e

# Bring pasta0 up (pasta creates it but doesn't bring it up without --config-net)
ip link set {pasta_dev} up

# Create L2 bridge — connects pasta0 and Firecracker TAP
ip link add {bridge} type bridge
ip link set {bridge} up

# Add pasta's TAP to bridge (pasta created this device)
ip link set {pasta_dev} master {bridge}

# Add Firecracker's TAP to bridge
ip link set {fc_tap} master {bridge}

# Add IP to bridge for health checks (namespace needs route to reach guest)
ip addr add {namespace_ip}/24 dev {bridge}

# Enable IP forwarding
echo 1 > /proc/sys/net/ipv4/ip_forward
"#,
            bridge = BRIDGE_DEVICE,
            pasta_dev = self.pasta_device,
            fc_tap = self.tap_device,
            namespace_ip = NAMESPACE_IP,
        );

        script
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

    /// Detect host's global IPv6 address for pasta outbound traffic
    fn detect_host_ipv6() -> Option<String> {
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

        // Wait for PID file to appear (signals pasta is ready)
        let deadline = std::time::Instant::now() + PASTA_READY_TIMEOUT;
        loop {
            if pid_file.exists() {
                info!("pasta ready (PID file created)");
                break;
            }

            // Check if pasta died during startup
            match child.try_wait() {
                Ok(Some(status)) => {
                    // Give the stderr reader a moment to drain the pipe so the
                    // error includes what pasta actually printed.
                    tokio::time::sleep(PASTA_STDERR_DRAIN_DELAY).await;
                    anyhow::bail!(
                        "pasta exited before becoming ready (status: {}){}",
                        status,
                        self.stderr_tail_message()
                    );
                }
                Ok(None) => {} // Still running
                Err(e) => anyhow::bail!("failed to check pasta status: {}", e),
            }

            if std::time::Instant::now() > deadline {
                let _ = child.kill().await;
                anyhow::bail!(
                    "pasta did not become ready within {:?}{}",
                    PASTA_READY_TIMEOUT,
                    self.stderr_tail_message()
                );
            }

            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
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

            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
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
            script = %bridge_script.lines().filter(|l| !l.trim().is_empty() && !l.trim().starts_with('#')).collect::<Vec<_>>().join("; "),
            "running bridge setup script"
        );

        let output = Command::new(&nsenter_prefix[0])
            .args(&nsenter_prefix[1..])
            .arg("bash")
            .arg("-c")
            .arg(&bridge_script)
            .output()
            .await
            .context("running bridge setup via nsenter")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("bridge setup failed: {}", stderr.trim());
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

    async fn cleanup(&mut self) -> Result<()> {
        info!(vm_id = %self.vm_id, "cleaning up pasta resources");

        if let Some(mut process) = self.pasta_process.take() {
            if let Err(e) = process.kill().await {
                warn!("failed to kill pasta: {}", e);
            }
            let _ = process.wait().await;
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
}
