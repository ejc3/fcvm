//! Race-free TCP generation boundaries for memory snapshots.
//!
//! A restore-time socket dump is too late: the restored workload and an early
//! published-port client can create sockets before the dump.  A tuple is also
//! not a socket identity, so killing by tuple can destroy a replacement socket.
//! Every memory snapshot therefore carries a manifest captured behind a NEW-flow
//! firewall gate.  The manifest records kernel socket cookies, and restore uses
//! cookie-bound `SOCK_DESTROY` requests with netlink acknowledgements.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

const MANIFEST_VERSION: u32 = 1;
const MANIFEST_PATH: &str = "/run/fcvm/snapshot-network.json";
pub(crate) const SYSFS_NET_PATH: &str = "/sys/class/net";
const BOUNDARY_LOCK_PATH: &str = "/run/fcvm/snapshot-network.lock";
const GATE_INPUT_CHAIN: &str = "FCVM_SNAPSHOT_IN";
const GATE_OUTPUT_CHAIN: &str = "FCVM_SNAPSHOT_OUT";

const NETLINK_SOCK_DIAG: i32 = 4;
const SOCK_DIAG_BY_FAMILY: u16 = 20;
const SOCK_DESTROY: u16 = 21;
const NLMSG_NOOP: u16 = 1;
const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const NLMSG_OVERRUN: u16 = 4;
const NLM_F_REQUEST: u16 = 0x01;
const NLM_F_ACK: u16 = 0x04;
const NLM_F_DUMP: u16 = 0x300;
const NLM_F_DUMP_INTR: u16 = 0x10;
const IPPROTO_TCP: u8 = 6;
const TCP_LISTEN: u8 = 10;
const TCP_STATE_MAX: u8 = 12;
const INET_DIAG_NOCOOKIE: u32 = u32::MAX;

static NEXT_NETLINK_SEQUENCE: AtomicU32 = AtomicU32::new(1);
/// Distinct from the netlink sequence: this only has to make a temporary
/// manifest filename unique within one process, and sharing the netlink counter
/// coupled two unrelated identifiers.
static NEXT_MANIFEST_TEMP_ID: AtomicU32 = AtomicU32::new(1);

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct NetlinkHeader {
    length: u32,
    message_type: u16,
    flags: u16,
    sequence: u32,
    port_id: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct InetDiagSockId {
    source_port_be: u16,
    destination_port_be: u16,
    source: [u32; 4],
    destination: [u32; 4],
    interface_id: u32,
    cookie: [u32; 2],
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct InetDiagRequest {
    family: u8,
    protocol: u8,
    extensions: u8,
    pad: u8,
    states: u32,
    id: InetDiagSockId,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct InetDiagMessage {
    family: u8,
    state: u8,
    timer: u8,
    retransmits: u8,
    id: InetDiagSockId,
    expires: u32,
    receive_queue: u32,
    write_queue: u32,
    uid: u32,
    inode: u32,
}

const _: () = assert!(std::mem::size_of::<NetlinkHeader>() == 16);
const _: () = assert!(std::mem::size_of::<InetDiagSockId>() == 48);
const _: () = assert!(std::mem::size_of::<InetDiagRequest>() == 56);
const _: () = assert!(std::mem::size_of::<InetDiagMessage>() == 72);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct TcpSocketIdentity {
    family: u8,
    state: u8,
    id: InetDiagSockId,
}

impl TcpSocketIdentity {
    fn peer_ip(&self) -> Result<IpAddr> {
        match self.family as i32 {
            libc::AF_INET => Ok(IpAddr::V4(Ipv4Addr::from(
                self.id.destination[0].to_ne_bytes(),
            ))),
            libc::AF_INET6 => {
                let mut bytes = [0u8; 16];
                for (index, word) in self.id.destination.iter().enumerate() {
                    bytes[index * 4..index * 4 + 4].copy_from_slice(&word.to_ne_bytes());
                }
                Ok(IpAddr::V6(Ipv6Addr::from(bytes)))
            }
            family => bail!("unsupported socket family in snapshot manifest: {family}"),
        }
    }

    fn is_preserved(&self) -> Result<bool> {
        Ok(match self.peer_ip()? {
            IpAddr::V4(address) => address.is_loopback(),
            IpAddr::V6(address) => address.is_loopback(),
        })
    }

    fn describe(&self) -> String {
        let peer = self
            .peer_ip()
            .map_or_else(|_| format!("family={}", self.family), |ip| ip.to_string());
        format!(
            "peer={}:{} cookie={:08x}:{:08x} state={}",
            peer,
            u16::from_be(self.id.destination_port_be),
            self.id.cookie[0],
            self.id.cookie[1],
            self.state
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SnapshotNetworkManifest {
    version: u32,
    sockets: Vec<TcpSocketIdentity>,
    /// Routes the kernel will NOT put back when the external link comes up again.
    ///
    /// Taking the link down purges the whole route table. Measured on a bridged
    /// guest (Graviton3, 6.18.44): after down/up the kernel restores only its
    /// own `proto kernel` entries (the connected subnet and `fe80::/64`), while
    /// `default via <gateway>` and the MMDS route `169.254.169.254` are gone for
    /// good. The default route is the guest's only path off-box, and the MMDS
    /// route is how fc-agent reads its restore epoch, so the boundary has to
    /// carry them across itself.
    #[serde(default)]
    routes: Vec<CapturedRoute>,
}

/// One route to reinstate after the boundary, with the family that owns it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct CapturedRoute {
    /// 4 or 6: `ip route` needs the family for anything but IPv4.
    family: u8,
    /// The route exactly as `ip route show` printed it, which is also the
    /// argument form `ip route replace` accepts.
    spec: String,
}

trait ManifestStore {
    fn save(&self, manifest: &SnapshotNetworkManifest) -> Result<()>;
    fn load(&self) -> Result<SnapshotNetworkManifest>;
    fn remove(&self) -> Result<()>;
}

struct FileManifestStore {
    path: PathBuf,
}

fn acquire_boundary_lock_at(path: &Path) -> Result<std::fs::File> {
    use fs2::FileExt;
    use std::os::unix::fs::OpenOptionsExt;

    let parent = path
        .parent()
        .context("snapshot network lock has no parent directory")?;
    std::fs::create_dir_all(parent).with_context(|| {
        format!(
            "creating snapshot network lock directory {}",
            parent.display()
        )
    })?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("opening snapshot network lock {}", path.display()))?;
    FileExt::lock_exclusive(&file)
        .with_context(|| format!("locking snapshot network transaction {}", path.display()))?;
    Ok(file)
}

fn acquire_boundary_lock() -> Result<std::fs::File> {
    acquire_boundary_lock_at(Path::new(BOUNDARY_LOCK_PATH))
}

impl Default for FileManifestStore {
    fn default() -> Self {
        Self {
            path: PathBuf::from(MANIFEST_PATH),
        }
    }
}

impl ManifestStore for FileManifestStore {
    fn save(&self, manifest: &SnapshotNetworkManifest) -> Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

        let parent = self
            .path
            .parent()
            .context("snapshot network manifest has no parent directory")?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
        let temporary = parent.join(format!(
            ".snapshot-network.{}.{}.tmp",
            std::process::id(),
            NEXT_MANIFEST_TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let bytes = serde_json::to_vec(manifest).context("serializing snapshot socket manifest")?;
        let write_result = (|| -> Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)
                .with_context(|| format!("creating {}", temporary.display()))?;
            file.write_all(&bytes)
                .with_context(|| format!("writing {}", temporary.display()))?;
            file.sync_all()
                .with_context(|| format!("syncing {}", temporary.display()))?;
            std::fs::rename(&temporary, &self.path).with_context(|| {
                format!(
                    "atomically publishing snapshot network manifest {}",
                    self.path.display()
                )
            })?;
            Ok(())
        })();
        if write_result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        write_result
    }

    fn load(&self) -> Result<SnapshotNetworkManifest> {
        let bytes = std::fs::read(&self.path).with_context(|| {
            format!(
                "reading snapshot-time socket manifest {}. This snapshot has no proven \
                 TCP generation boundary; recreate it with the current fcvm",
                self.path.display()
            )
        })?;
        let manifest: SnapshotNetworkManifest =
            serde_json::from_slice(&bytes).context("parsing snapshot-time socket manifest")?;
        if manifest.version != MANIFEST_VERSION {
            bail!(
                "unsupported snapshot network manifest version {} (expected {}); recreate snapshot",
                manifest.version,
                MANIFEST_VERSION
            );
        }
        for socket in &manifest.sockets {
            if socket.id.cookie == [INET_DIAG_NOCOOKIE; 2] {
                bail!(
                    "snapshot socket {} has no kernel cookie; refusing tuple-only cleanup",
                    socket.describe()
                );
            }
            socket.peer_ip()?;
        }
        Ok(manifest)
    }

    fn remove(&self) -> Result<()> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error)
                .with_context(|| format!("removing snapshot manifest {}", self.path.display())),
        }
    }
}

trait FirewallGate {
    async fn close(&self) -> Result<()>;
    async fn open(&self) -> Result<()>;
    /// Report whether the gate is already fully installed: both directional
    /// chains present with exactly the boundary rules, each hooked from its
    /// built-in chain, in both address families.
    async fn is_closed(&self) -> Result<bool>;
}

trait ExternalLink {
    async fn down(&self) -> Result<()>;
    async fn up(&self) -> Result<()>;
}

trait PacketReceiveBarrier {
    fn synchronize(&self) -> Result<()>;
}

trait RouteTable {
    /// Read the routes that must be reinstated after the link returns.
    async fn capture(&self) -> Result<Vec<CapturedRoute>>;
    /// Reinstate them. Idempotent: `ip route replace` accepts a route that is
    /// already present, so a partial restore can simply be repeated.
    async fn reinstate(&self, routes: &[CapturedRoute]) -> Result<()>;
}

trait CommandRunner {
    async fn output(&self, program: &str, args: &[&str]) -> io::Result<std::process::Output>;

    /// Run with `input` piped to stdin. Exists for the batch tools
    /// (`iptables-restore`, `ip -batch -`) that let one process spawn carry
    /// what would otherwise be a dozen: on a freshly restored clone every
    /// spawn faults its text and libraries back in through the memory
    /// backend, so spawn count is the boundary's dominant cost.
    async fn output_with_input(
        &self,
        program: &str,
        args: &[&str],
        input: &[u8],
    ) -> io::Result<std::process::Output>;
}

struct SystemCommandRunner;

impl CommandRunner for SystemCommandRunner {
    async fn output(&self, program: &str, args: &[&str]) -> io::Result<std::process::Output> {
        Command::new(program).args(args).output().await
    }

    async fn output_with_input(
        &self,
        program: &str,
        args: &[&str],
        input: &[u8],
    ) -> io::Result<std::process::Output> {
        use tokio::io::AsyncWriteExt;

        let mut child = Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        let mut stdin = child.stdin.take().ok_or_else(|| {
            io::Error::other(format!("{program} child has no stdin despite piped()"))
        })?;
        stdin.write_all(input).await?;
        drop(stdin);
        child.wait_with_output().await
    }
}

/// Name the guest's one external interface by asking sysfs which netdev is
/// backed by a bus device.
///
/// The name is not fixed across hypervisors, so hardcoding one silently breaks
/// the other: Firecracker's MMIO virtio-net keeps the kernel's `eth0`, while
/// Cloud Hypervisor's PCI virtio-net is renamed by udev under predictable
/// naming to `enp0s4` (both measured). The kernel `ip=` boot argument names
/// `eth0` because it runs before that rename, so the address survives and every
/// later by-name operation does not.
///
/// Only `lo` lacks the `device` symlink, so this also stays correct if the netns
/// ever gains a bridge or veth. Unlike reading an address or a default route it
/// is valid mid-boundary, where the link is down and the routes are purged.
fn external_interface_in(sysfs_net: &Path) -> Result<String> {
    let entries = std::fs::read_dir(sysfs_net)
        .with_context(|| format!("listing network interfaces in {}", sysfs_net.display()))?;
    let mut all = Vec::new();
    let mut backed = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| {
            format!(
                "reading a network interface entry in {}",
                sysfs_net.display()
            )
        })?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.path().join("device").exists() {
            backed.push(name.clone());
        }
        all.push(name);
    }
    all.sort();
    backed.sort();
    match backed.len() {
        1 => Ok(backed.remove(0)),
        _ => bail!(
            "expected exactly one device-backed external interface, found {:?} among {:?}",
            backed,
            all
        ),
    }
}

fn external_interface() -> Result<String> {
    external_interface_in(Path::new(SYSFS_NET_PATH))
}

/// The guest's one external interface, driven through direct syscalls.
struct IpLink {
    interface: String,
}

impl IpLink {
    fn new(interface: String) -> Self {
        Self { interface }
    }
}

/// Set or clear IFF_UP on an interface with SIOCSIFFLAGS.
///
/// The kernel call `ip link set dev X up|down` makes, without the process
/// spawn. The boundary drives the link on every clone restore, where a spawn
/// costs far more than the syscall: the restored process image faults its
/// text and libraries back in through the memory backend. Spawn-free is also
/// what makes re-asserting the link-down on every restore affordable, and
/// that re-assert is what keeps the cleanup sound against a privileged
/// workload sharing this namespace.
fn set_interface_up(interface: &str, up: bool) -> Result<()> {
    use std::os::fd::AsRawFd;

    // `as _` on the request numbers is load-bearing across targets: the guest
    // binary is musl, whose `ioctl` takes an i32 request, while glibc takes a
    // c_ulong. A host-target build hides that difference entirely.

    let fd = crate::network::open_raw_socket(libc::AF_INET, libc::SOCK_DGRAM, 0)
        .context("opening a control socket for the snapshot network link")?;
    // SAFETY: ifreq is a plain C struct; zeroing is its valid empty state.
    let mut request: libc::ifreq = unsafe { std::mem::zeroed() };
    let name = interface.as_bytes();
    if name.len() >= request.ifr_name.len() {
        bail!("interface name {interface:?} does not fit in an ifreq");
    }
    for (slot, byte) in request.ifr_name.iter_mut().zip(name) {
        *slot = *byte as libc::c_char;
    }
    // SAFETY: the fd is a live AF_INET socket and `request` is initialized
    // with a NUL-terminated name; SIOCGIFFLAGS fills the flags union member.
    if unsafe { libc::ioctl(fd.as_raw_fd(), libc::SIOCGIFFLAGS as _, &mut request) } < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("reading interface flags for {interface}"));
    }
    // SAFETY: SIOCGIFFLAGS just populated the flags member of the union.
    let flags = unsafe { request.ifr_ifru.ifru_flags } as libc::c_int;
    let updated = if up {
        flags | libc::IFF_UP
    } else {
        flags & !libc::IFF_UP
    };
    request.ifr_ifru.ifru_flags = updated as libc::c_short;
    // SAFETY: same fd and struct, now carrying the flags to install.
    if unsafe { libc::ioctl(fd.as_raw_fd(), libc::SIOCSIFFLAGS as _, &request) } < 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| {
            format!(
                "setting interface {interface} {}",
                if up { "up" } else { "down" }
            )
        });
    }
    Ok(())
}

impl ExternalLink for IpLink {
    async fn down(&self) -> Result<()> {
        set_interface_up(&self.interface, false)
    }

    async fn up(&self) -> Result<()> {
        set_interface_up(&self.interface, true)
    }
}

struct IpRoutes<R> {
    runner: R,
}

impl<R> IpRoutes<R> {
    fn new(runner: R) -> Self {
        Self { runner }
    }
}

impl<R: CommandRunner + Sync> IpRoutes<R> {
    async fn run(&self, args: &[&str]) -> Result<std::process::Output> {
        let output = self
            .runner
            .output("ip", args)
            .await
            .context("spawning ip to read or reinstate the snapshot route table")?;
        if !output.status.success() {
            bail!(
                "snapshot network route command failed: {}",
                command_failure("ip", args, &output)
            );
        }
        Ok(output)
    }

    async fn capture_family(&self, family: u8, routes: &mut Vec<CapturedRoute>) -> Result<()> {
        let flag = if family == 6 { "-6" } else { "-4" };
        let output = self.run(&[flag, "route", "show"]).await?;
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let spec = line.trim();
            if spec.is_empty() {
                continue;
            }
            // `proto kernel` routes come back on their own when the link does,
            // measured on both families, so reinstating them is noise at best.
            if spec.contains("proto kernel") {
                continue;
            }
            // A route the kernel is aging out cannot be re-added verbatim:
            // `expires` is state, not an argument `ip route replace` accepts.
            if spec.contains("expires") {
                continue;
            }
            routes.push(CapturedRoute {
                family,
                spec: spec.to_string(),
            });
        }
        Ok(())
    }
}

impl<R: CommandRunner + Sync> RouteTable for IpRoutes<R> {
    async fn capture(&self) -> Result<Vec<CapturedRoute>> {
        let mut routes = Vec::new();
        self.capture_family(4, &mut routes).await?;
        self.capture_family(6, &mut routes).await?;
        Ok(routes)
    }

    async fn reinstate(&self, routes: &[CapturedRoute]) -> Result<()> {
        // One `ip -batch -` spawn per family instead of one per route: the
        // family flag is global to the ip invocation, and each avoided spawn
        // is restore latency on a cold clone.
        for family in [4u8, 6u8] {
            let lines: Vec<String> = routes
                .iter()
                .filter(|route| route.family == family)
                .map(|route| format!("route replace {}", route.spec))
                .collect();
            if lines.is_empty() {
                continue;
            }
            let flag = if family == 6 { "-6" } else { "-4" };
            let payload = lines.join("\n") + "\n";
            let args = [flag, "-batch", "-"];
            let output = self
                .runner
                .output_with_input("ip", &args, payload.as_bytes())
                .await
                .context("spawning ip to reinstate the snapshot route table")?;
            if !output.status.success() {
                bail!(
                    "snapshot network route batch failed: {} (batch input {payload:?})",
                    command_failure("ip", &args, &output)
                );
            }
        }
        Ok(())
    }
}

struct SystemPacketReceiveBarrier;

impl PacketReceiveBarrier for SystemPacketReceiveBarrier {
    fn synchronize(&self) -> Result<()> {
        const ETH_P_ALL: u16 = 0x0003;

        // packet_release() executes synchronize_net(), whose kernel contract is
        // to wait until every packet already in receive processing is done.
        // The NEW-flow gate is installed first and the link is down, so packets
        // that begin after this grace period cannot create an uncaptured TCP
        // socket. Merely waiting or taking repeated dumps cannot prove this.
        // Dropping the fd invokes the packet socket release path and its
        // synchronize_net() grace period.
        let fd = crate::network::open_raw_socket(
            libc::AF_PACKET,
            libc::SOCK_RAW,
            ETH_P_ALL.to_be() as i32,
        )
        .context("opening AF_PACKET receive-path barrier (guest kernel needs CONFIG_PACKET=y)")?;
        drop(fd);
        Ok(())
    }
}

struct IptablesGate<R> {
    runner: R,
    /// Per-program `-S` parse retained from verification for the removal that
    /// follows it in the same transaction, dropped by every gate mutation
    /// this module makes. No other fc-agent transaction can invalidate it:
    /// they serialize on the guest-wide boundary lock. A privileged workload
    /// can, and then the computed removal names a rule that no longer
    /// matches, which fails the batch and fails the restore closed.
    /// Guards the map only, never held across an await.
    state_cache: std::sync::Mutex<std::collections::HashMap<String, FamilyGateState>>,
}

impl<R> IptablesGate<R> {
    fn new(runner: R) -> Self {
        Self {
            runner,
            state_cache: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }
}

fn command_failure(program: &str, args: &[&str], output: &std::process::Output) -> String {
    format!(
        "{} {} exited {} (stdout={:?}, stderr={:?})",
        program,
        args.join(" "),
        output
            .status
            .code()
            .map_or_else(|| "by signal".to_string(), |code| code.to_string()),
        String::from_utf8_lossy(&output.stdout).trim(),
        String::from_utf8_lossy(&output.stderr).trim(),
    )
}

/// The loopback exemptions the two families preserve across the boundary.
const IPV4_LOOPBACK: &str = "127.0.0.0/8";
const IPV6_LOOPBACK: &str = "::1/128";

/// The exact rule bodies a gate chain carries, without the `-A <chain>`
/// prefix. Shared by installation and verification so the two can never
/// drift: what close() appends is literally what is_closed() expects.
fn gate_chain_rules(loopback_flag: &str, loopback_network: &str) -> [Vec<String>; 3] {
    let owned = |tokens: &[&str]| tokens.iter().map(|token| token.to_string()).collect();
    [
        owned(&["-p", "tcp", loopback_flag, loopback_network, "-j", "RETURN"]),
        owned(&[
            "-p",
            "tcp",
            "-m",
            "conntrack",
            "--ctstate",
            "NEW",
            "-j",
            "REJECT",
            "--reject-with",
            "tcp-reset",
        ]),
        owned(&["-j", "RETURN"]),
    ]
}

/// The one rule that routes a built-in chain into a gate chain, as `-S`
/// prints it. Shared by installation, verification, and removal so the three
/// can never drift: a hook changed in one of them alone would silently kill
/// the verified fast path (verification stops matching) and break removal
/// (`-X` on a still-referenced chain).
fn hook_jump_args(chain: &str) -> [&str; 2] {
    ["-j", chain]
}

fn hook_install_args<'a>(hook: &'a str, chain: &'a str) -> [&'a str; 6] {
    let [jump, target] = hook_jump_args(chain);
    ["-w", "-I", hook, "1", jump, target]
}

fn hook_check_args<'a>(hook: &'a str, chain: &'a str) -> [&'a str; 5] {
    let [jump, target] = hook_jump_args(chain);
    ["-w", "-C", hook, jump, target]
}

/// One family's observed gate state, parsed from a single `iptables -S` dump.
///
/// `None` rule lists mean the chain is not declared at all; an empty list is a
/// declared-but-empty chain. Hook counts tally `-A INPUT -j <chain>` (and the
/// OUTPUT twin) exactly — a jump carrying extra matches is not our hook.
/// The first-rule flags record whether the hook is the built-in chain's FIRST
/// rule (`-S` prints rules in evaluation order): the gate only governs packets
/// that reach it, so a hook shadowed by any earlier rule is not a closed gate.
#[derive(Debug, Default, PartialEq, Eq)]
struct FamilyGateState {
    input_rules: Option<Vec<Vec<String>>>,
    output_rules: Option<Vec<Vec<String>>>,
    input_hooks: usize,
    output_hooks: usize,
    input_hook_is_first_rule: bool,
    output_hook_is_first_rule: bool,
}

fn parse_gate_state(dump: &str) -> FamilyGateState {
    let mut state = FamilyGateState::default();
    let mut input_rules_seen = 0usize;
    let mut output_rules_seen = 0usize;
    for line in dump.lines() {
        let tokens: Vec<String> = line.split_whitespace().map(str::to_string).collect();
        let [command, chain, rest @ ..] = tokens.as_slice() else {
            continue;
        };
        match (command.as_str(), chain.as_str()) {
            ("-N", GATE_INPUT_CHAIN) => {
                state.input_rules.get_or_insert_with(Vec::new);
            }
            ("-N", GATE_OUTPUT_CHAIN) => {
                state.output_rules.get_or_insert_with(Vec::new);
            }
            ("-A", GATE_INPUT_CHAIN) => {
                state
                    .input_rules
                    .get_or_insert_with(Vec::new)
                    .push(rest.to_vec());
            }
            ("-A", GATE_OUTPUT_CHAIN) => {
                state
                    .output_rules
                    .get_or_insert_with(Vec::new)
                    .push(rest.to_vec());
            }
            ("-A", "INPUT") => {
                input_rules_seen += 1;
                if rest == hook_jump_args(GATE_INPUT_CHAIN) {
                    state.input_hooks += 1;
                    if input_rules_seen == 1 {
                        state.input_hook_is_first_rule = true;
                    }
                }
            }
            ("-A", "OUTPUT") => {
                output_rules_seen += 1;
                if rest == hook_jump_args(GATE_OUTPUT_CHAIN) {
                    state.output_hooks += 1;
                    if output_rules_seen == 1 {
                        state.output_hook_is_first_rule = true;
                    }
                }
            }
            _ => {}
        }
    }
    state
}

/// Compare one rule as a sorted token multiset. `iptables -S` reorders
/// arguments relative to what was appended (measured on iptables v1.8.10
/// nf_tables: `-A C -p tcp -s 127.0.0.0/8 -j RETURN` prints as
/// `-A C -s 127.0.0.0/8 -p tcp -j RETURN`), and pinning the exact print order
/// would couple verification to one iptables version's renderer.
fn sorted_tokens(tokens: &[String]) -> Vec<String> {
    let mut sorted = tokens.to_vec();
    sorted.sort();
    sorted
}

/// Whether one family's observed state is the fully armed gate: both chains
/// carrying exactly the boundary rules, each hooked as the FIRST rule of its
/// built-in chain. First position is part of the invariant — close() inserts
/// with `-I 1`, and a hook shadowed by any earlier rule would let that rule
/// admit NEW flows the manifest never captured. Extra duplicate hooks below
/// the first are still closed (the gate verdict is unchanged); any deviation
/// inside a chain is not.
///
/// Scope: this sees exactly what `iptables -S` renders. A rule injected with
/// native nft tooling into a separate higher-priority nft chain is invisible
/// here and bypasses the gate at packet time regardless; that holds equally
/// for the unconditional re-assert path, which installs into the same
/// iptables-visible chains.
fn family_gate_is_closed(state: &FamilyGateState, loopback_network: &str) -> bool {
    let chain_matches = |observed: &Option<Vec<Vec<String>>>, loopback_flag: &str| {
        let expected = gate_chain_rules(loopback_flag, loopback_network);
        observed.as_ref().is_some_and(|rules| {
            rules.len() == expected.len()
                && rules
                    .iter()
                    .zip(expected.iter())
                    .all(|(observed_rule, expected_rule)| {
                        sorted_tokens(observed_rule) == sorted_tokens(expected_rule)
                    })
        })
    };
    chain_matches(&state.input_rules, "-s")
        && chain_matches(&state.output_rules, "-d")
        && state.input_hook_is_first_rule
        && state.output_hook_is_first_rule
}

impl<R: CommandRunner> IptablesGate<R> {
    async fn required(&self, program: &str, args: &[&str]) -> Result<()> {
        let output = self
            .runner
            .output(program, args)
            .await
            .with_context(|| format!("spawning {program}"))?;
        if !output.status.success() {
            bail!(
                "snapshot network gate command failed: {}",
                command_failure(program, args, &output)
            );
        }
        Ok(())
    }

    async fn chain_exists(&self, program: &str, chain: &str) -> Result<bool> {
        let args = ["-w", "-L", chain];
        let output = self.runner.output(program, &args).await?;
        Ok(output.status.success())
    }

    async fn read_family_state(&self, program: &str) -> Result<FamilyGateState> {
        let args = ["-w", "-S"];
        let output = self
            .runner
            .output(program, &args)
            .await
            .with_context(|| format!("spawning {program}"))?;
        if !output.status.success() {
            bail!(
                "snapshot network gate state read failed: {}",
                command_failure(program, &args, &output)
            );
        }
        Ok(parse_gate_state(&String::from_utf8_lossy(&output.stdout)))
    }

    async fn family_is_closed(&self, program: &str, loopback_network: &str) -> Result<bool> {
        let state = self.read_family_state(program).await?;
        let closed = family_gate_is_closed(&state, loopback_network);
        // Retain the parse for the removal that follows verification in the
        // same locked transaction, saving that removal's own `-S` spawn.
        self.state_cache
            .lock()
            .unwrap()
            .insert(program.to_string(), state);
        Ok(closed)
    }

    async fn configure_gate_chain(
        &self,
        program: &str,
        chain: &str,
        loopback_flag: &str,
        loopback_network: &str,
    ) -> Result<()> {
        if !self.chain_exists(program, chain).await? {
            self.required(program, &["-w", "-N", chain]).await?;
        }
        self.required(program, &["-w", "-F", chain]).await?;
        for rule in gate_chain_rules(loopback_flag, loopback_network) {
            let mut args: Vec<&str> = vec!["-w", "-A", chain];
            args.extend(rule.iter().map(String::as_str));
            self.required(program, &args).await?;
        }
        Ok(())
    }

    async fn install_hook(&self, program: &str, hook: &str, chain: &str) -> Result<()> {
        // Always insert at position 1. `-C` can only prove the jump exists
        // SOMEWHERE, and a jump below a foreign rule is a gate that foreign
        // rule can bypass, so close() must put a copy ahead of everything.
        // A duplicate left lower in the chain is harmless (the first match
        // governs) and remove_family retires every counted copy.
        self.required(program, &hook_install_args(hook, chain))
            .await?;
        self.required(program, &hook_check_args(hook, chain))
            .await?;
        Ok(())
    }

    async fn install_family(&self, program: &str, loopback_network: &str) -> Result<()> {
        // INPUT and OUTPUT cannot share one chain: allowing destination
        // 10.0.2.0/24 in a shared chain allowed every packet addressed to the
        // guest, and allowing its source subnet allowed every outbound packet.
        // Direction-specific chains preserve loopback only; gateway/NFS
        // sockets are restored explicitly after the clone gate opens.
        self.configure_gate_chain(program, GATE_INPUT_CHAIN, "-s", loopback_network)
            .await?;
        self.configure_gate_chain(program, GATE_OUTPUT_CHAIN, "-d", loopback_network)
            .await?;
        self.install_hook(program, "INPUT", GATE_INPUT_CHAIN)
            .await?;
        self.install_hook(program, "OUTPUT", GATE_OUTPUT_CHAIN)
            .await?;
        // This family's rules just changed; a parse retained from an earlier
        // verification no longer describes them.
        self.state_cache.lock().unwrap().remove(program);
        Ok(())
    }

    /// Remove one family's gate with one `iptables-restore --noflush` batch,
    /// computed from a single `-S` read: exactly the counted hook deletions,
    /// then flush and delete each chain that exists. When the same locked
    /// transaction already read the state to verify, that parse is reused and
    /// the removal spends no read at all. Explicit `-D`/`-F`/`-X` lines in
    /// restore input are accepted (verified live on the guest's iptables
    /// v1.8.10 nf_tables build, which removed both duplicate jumps and the
    /// chain in one commit). One spawn instead of a dozen matters here: on a
    /// freshly restored clone every spawn faults its pages back in through
    /// the memory backend.
    ///
    /// The plan is computed rather than probed, so it assumes the observed
    /// state still holds. Other fc-agent transactions cannot break that (the
    /// guest-wide lock serializes them); a privileged workload sharing this
    /// namespace can, and then a `-D` or `-X` names something that no longer
    /// matches and the batch fails, which fails the restore closed.
    async fn remove_family(&self, program: &str) -> Result<()> {
        // Bounded: a hook carrying more duplicate jumps than any interrupted
        // sequence could leave is tampering, not drift, and deleting without
        // bound would hang restore instead of reporting it.
        const MAX_DUPLICATE_HOOKS: usize = 64;
        let cached = self.state_cache.lock().unwrap().remove(program);
        let state = match cached {
            Some(state) => state,
            None => self.read_family_state(program).await?,
        };
        let mut lines = vec!["*filter".to_string()];
        let hooks = [
            ("INPUT", GATE_INPUT_CHAIN, state.input_hooks),
            ("OUTPUT", GATE_OUTPUT_CHAIN, state.output_hooks),
        ];
        for (hook, chain, count) in hooks {
            if count > MAX_DUPLICATE_HOOKS {
                bail!(
                    "{program} {hook} jumps to {chain} {count} times \
                     (limit {MAX_DUPLICATE_HOOKS}); refusing cleanup"
                );
            }
            for _ in 0..count {
                lines.push(format!("-D {hook} {}", hook_jump_args(chain).join(" ")));
            }
        }
        let chains = [
            (GATE_INPUT_CHAIN, state.input_rules.is_some()),
            (GATE_OUTPUT_CHAIN, state.output_rules.is_some()),
        ];
        for (chain, declared) in chains {
            if declared {
                lines.push(format!("-F {chain}"));
                lines.push(format!("-X {chain}"));
            }
        }
        if lines.len() == 1 {
            return Ok(());
        }
        lines.push("COMMIT".to_string());
        let payload = lines.join("\n") + "\n";
        let restore_program = format!("{program}-restore");
        let args = ["-w", "--noflush"];
        let output = self
            .runner
            .output_with_input(&restore_program, &args, payload.as_bytes())
            .await
            .with_context(|| format!("spawning {restore_program}"))?;
        if !output.status.success() {
            bail!(
                "snapshot network gate batch removal failed: {} (batch input {payload:?})",
                command_failure(&restore_program, &args, &output)
            );
        }
        Ok(())
    }
}

impl<R: CommandRunner + Sync> FirewallGate for IptablesGate<R> {
    async fn close(&self) -> Result<()> {
        self.install_family("iptables", IPV4_LOOPBACK)
            .await
            .context("installing IPv4 snapshot network gate")?;
        self.install_family("ip6tables", IPV6_LOOPBACK)
            .await
            .context("installing IPv6 snapshot network gate")
    }

    async fn open(&self) -> Result<()> {
        // Mutations stay sequential on purpose. iptables-nft has no shared
        // userspace lock (`-w` is a no-op there) and commits race the
        // kernel's per-netns generation counter, where a lost retry surfaces
        // as EAGAIN; through required() that would fail the restore and kill
        // the clone, a poor trade for one process spawn of overlap. Both
        // verdicts are still collected so one family's failure cannot mask
        // the other's.
        let ipv4 = self.remove_family("iptables").await;
        let ipv6 = self.remove_family("ip6tables").await;
        match (ipv4, ipv6) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(a), Ok(())) => Err(a),
            (Ok(()), Err(b)) => Err(b),
            (Err(a), Err(b)) => Err(anyhow::anyhow!(
                "IPv4 gate cleanup: {a:#}; IPv6 gate cleanup: {b:#}"
            )),
        }
    }

    async fn is_closed(&self) -> Result<bool> {
        // Reads only: safe to overlap regardless of backend.
        let (ipv4, ipv6) = tokio::join!(
            self.family_is_closed("iptables", IPV4_LOOPBACK),
            self.family_is_closed("ip6tables", IPV6_LOOPBACK)
        );
        Ok(ipv4? && ipv6?)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DestroyOutcome {
    Destroyed,
    AlreadyGone,
}

/// What the cookie-bound cleanup actually did, split by outcome.
///
/// A socket the kernel had already retired is a different fact from one this
/// cleanup destroyed, and only the split tells an operator whether the manifest
/// still describes the restored guest. Reporting one total for both hides a
/// manifest that has gone entirely stale behind a healthy-looking count.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct CleanupTally {
    destroyed: usize,
    already_gone: usize,
}

/// What one restore-side boundary transaction did, surfaced to the restore
/// orchestrator so the host-visible ACK telemetry can attribute the path
/// taken (verified fast path versus full re-assert).
#[derive(Debug, Clone, Copy)]
pub struct RestoreNetworkReport {
    /// True when the snapshot's armed boundary verified intact and the
    /// re-close/link-down/receive-barrier re-assert was skipped.
    pub verified_armed: bool,
    /// Milliseconds probing the gate rules and link flags.
    pub verify_ms: f64,
    /// Milliseconds re-asserting close/link-down/barrier; zero on the
    /// verified fast path.
    pub reassert_ms: f64,
    /// Milliseconds loading the manifest and retiring its sockets.
    pub destroy_ms: f64,
    /// Milliseconds removing the gate, raising the link, and reinstating
    /// routes.
    pub reopen_ms: f64,
    tally: CleanupTally,
}

/// Total budget for retiring every socket named by the manifest.
///
/// Each `SOCK_DESTROY` carries its own five-second receive timeout, so a large
/// manifest could otherwise hold an unpublished clone for minutes with no upper
/// bound. Exceeding the budget fails closed and names the progress made, which
/// is diagnosable; an open-ended stall is not.
const CLEANUP_BUDGET: std::time::Duration = std::time::Duration::from_secs(30);

trait SocketDiagnostic {
    fn dump_tcp_sockets(&mut self) -> Result<Vec<TcpSocketIdentity>>;
    fn destroy(&mut self, socket: &TcpSocketIdentity) -> Result<DestroyOutcome>;
}

struct SystemSocketDiagnostic;

impl SocketDiagnostic for SystemSocketDiagnostic {
    fn dump_tcp_sockets(&mut self) -> Result<Vec<TcpSocketIdentity>> {
        let mut sockets = Vec::new();
        for family in [libc::AF_INET as u8, libc::AF_INET6 as u8] {
            sockets.extend(dump_family(family)?);
        }
        Ok(sockets)
    }

    fn destroy(&mut self, socket: &TcpSocketIdentity) -> Result<DestroyOutcome> {
        destroy_socket(socket)
    }
}

fn read_unaligned<T: Copy>(bytes: &[u8]) -> Result<T> {
    if bytes.len() < std::mem::size_of::<T>() {
        bail!(
            "short netlink payload: {} bytes, need {}",
            bytes.len(),
            std::mem::size_of::<T>()
        );
    }
    // SAFETY: length was checked and read_unaligned permits any byte alignment.
    Ok(unsafe { bytes.as_ptr().cast::<T>().read_unaligned() })
}

fn append_netlink_header(bytes: &mut Vec<u8>, header: NetlinkHeader) {
    bytes.extend_from_slice(&header.length.to_ne_bytes());
    bytes.extend_from_slice(&header.message_type.to_ne_bytes());
    bytes.extend_from_slice(&header.flags.to_ne_bytes());
    bytes.extend_from_slice(&header.sequence.to_ne_bytes());
    bytes.extend_from_slice(&header.port_id.to_ne_bytes());
}

fn append_socket_id(bytes: &mut Vec<u8>, id: InetDiagSockId) {
    // Ports and addresses are already stored in their kernel UAPI byte order.
    // Serializing the integer's native representation preserves those bytes.
    bytes.extend_from_slice(&id.source_port_be.to_ne_bytes());
    bytes.extend_from_slice(&id.destination_port_be.to_ne_bytes());
    for address in id.source {
        bytes.extend_from_slice(&address.to_ne_bytes());
    }
    for address in id.destination {
        bytes.extend_from_slice(&address.to_ne_bytes());
    }
    bytes.extend_from_slice(&id.interface_id.to_ne_bytes());
    for cookie in id.cookie {
        bytes.extend_from_slice(&cookie.to_ne_bytes());
    }
}

fn append_diag_request(bytes: &mut Vec<u8>, request: InetDiagRequest) {
    bytes.extend_from_slice(&[
        request.family,
        request.protocol,
        request.extensions,
        request.pad,
    ]);
    bytes.extend_from_slice(&request.states.to_ne_bytes());
    append_socket_id(bytes, request.id);
}

#[cfg(test)]
fn append_diag_message(bytes: &mut Vec<u8>, message: InetDiagMessage) {
    bytes.extend_from_slice(&[
        message.family,
        message.state,
        message.timer,
        message.retransmits,
    ]);
    append_socket_id(bytes, message.id);
    bytes.extend_from_slice(&message.expires.to_ne_bytes());
    bytes.extend_from_slice(&message.receive_queue.to_ne_bytes());
    bytes.extend_from_slice(&message.write_queue.to_ne_bytes());
    bytes.extend_from_slice(&message.uid.to_ne_bytes());
    bytes.extend_from_slice(&message.inode.to_ne_bytes());
}

fn netlink_align(length: usize) -> usize {
    (length + 3) & !3
}

fn open_diag_socket() -> Result<OwnedFd> {
    use std::os::fd::AsRawFd;

    let fd = crate::network::open_raw_socket(libc::AF_NETLINK, libc::SOCK_RAW, NETLINK_SOCK_DIAG)
        .context("opening NETLINK_SOCK_DIAG socket (guest kernel needs CONFIG_INET_DIAG=y)")?;
    let mut address: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    address.nl_family = libc::AF_NETLINK as u16;
    // SAFETY: address points to a fully initialized sockaddr_nl.
    let bind_result = unsafe {
        libc::bind(
            fd.as_raw_fd(),
            (&address as *const libc::sockaddr_nl).cast(),
            std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        )
    };
    if bind_result < 0 {
        return Err(io::Error::last_os_error()).context("binding NETLINK_SOCK_DIAG socket");
    }
    // Connected netlink sockets can use send(2), avoiding a per-message
    // sockaddr and making every reply come from the kernel peer (pid zero).
    let connect_result = unsafe {
        libc::connect(
            fd.as_raw_fd(),
            (&address as *const libc::sockaddr_nl).cast(),
            std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        )
    };
    if connect_result < 0 {
        return Err(io::Error::last_os_error()).context("connecting NETLINK_SOCK_DIAG socket");
    }
    let timeout = libc::timeval {
        tv_sec: 5,
        tv_usec: 0,
    };
    let timeout_result = unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            (&timeout as *const libc::timeval).cast(),
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        )
    };
    if timeout_result < 0 {
        return Err(io::Error::last_os_error()).context("setting SOCK_DIAG receive timeout");
    }
    Ok(fd)
}

fn send_diag_request(
    fd: &OwnedFd,
    message_type: u16,
    flags: u16,
    request: &InetDiagRequest,
    sequence: u32,
) -> Result<()> {
    use std::os::fd::AsRawFd;

    let header = NetlinkHeader {
        length: (std::mem::size_of::<NetlinkHeader>() + std::mem::size_of::<InetDiagRequest>())
            as u32,
        message_type,
        flags,
        sequence,
        port_id: 0,
    };
    let mut bytes = Vec::with_capacity(header.length as usize);
    append_netlink_header(&mut bytes, header);
    append_diag_request(&mut bytes, *request);
    debug_assert_eq!(bytes.len(), header.length as usize);
    let sent = unsafe { libc::send(fd.as_raw_fd(), bytes.as_ptr().cast(), bytes.len(), 0) };
    if sent < 0 {
        return Err(io::Error::last_os_error()).context("sending SOCK_DIAG netlink request");
    }
    if sent as usize != bytes.len() {
        bail!(
            "short SOCK_DIAG send: wrote {sent} of {} bytes",
            bytes.len()
        );
    }
    Ok(())
}

fn receive_datagram(fd: &OwnedFd) -> Result<Vec<u8>> {
    use std::os::fd::AsRawFd;

    let mut bytes = vec![0u8; 256 * 1024];
    let mut peer: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    let mut iov = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: bytes.len(),
    };
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_name = (&mut peer as *mut libc::sockaddr_nl).cast();
    message.msg_namelen = std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t;
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    let received = unsafe { libc::recvmsg(fd.as_raw_fd(), &mut message, 0) };
    if received < 0 {
        return Err(io::Error::last_os_error()).context("receiving SOCK_DIAG netlink response");
    }
    if received == 0 {
        bail!("NETLINK_SOCK_DIAG returned EOF before completing request");
    }
    if message.msg_flags & libc::MSG_TRUNC != 0 {
        bail!(
            "SOCK_DIAG netlink datagram exceeded {} bytes; refusing a partial socket identity dump",
            bytes.len()
        );
    }
    if message.msg_namelen < std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t
        || peer.nl_family != libc::AF_NETLINK as u16
        || peer.nl_pid != 0
    {
        bail!(
            "SOCK_DIAG response did not come from the kernel peer (family={} pid={})",
            peer.nl_family,
            peer.nl_pid
        );
    }
    bytes.truncate(received as usize);
    Ok(bytes)
}

fn for_each_netlink_message(
    bytes: &[u8],
    sequence: u32,
    mut callback: impl FnMut(NetlinkHeader, &[u8]) -> Result<bool>,
) -> Result<bool> {
    let header_size = std::mem::size_of::<NetlinkHeader>();
    let mut offset = 0usize;
    while offset < bytes.len() {
        let header: NetlinkHeader = read_unaligned(&bytes[offset..])?;
        let length = header.length as usize;
        if length < header_size || offset + length > bytes.len() {
            bail!("invalid netlink message length {length} at offset {offset}");
        }
        if header.sequence == sequence {
            let payload = &bytes[offset + header_size..offset + length];
            if callback(header, payload)? {
                return Ok(true);
            }
        }
        offset = offset
            .checked_add(netlink_align(length))
            .context("netlink message offset overflow")?;
    }
    Ok(false)
}

fn decode_netlink_error(payload: &[u8]) -> Result<(i32, NetlinkHeader)> {
    let error = read_unaligned::<i32>(payload)?;
    let request = read_unaligned::<NetlinkHeader>(&payload[std::mem::size_of::<i32>()..])?;
    Ok((error, request))
}

fn validate_error_request(
    request: NetlinkHeader,
    sequence: u32,
    expected_message_type: u16,
) -> Result<()> {
    if request.sequence != sequence || request.message_type != expected_message_type {
        bail!(
            "netlink acknowledgement described the wrong request: type={} sequence={} \
             (expected type={} sequence={})",
            request.message_type,
            request.sequence,
            expected_message_type,
            sequence
        );
    }
    Ok(())
}

fn validate_dump_completion(header: NetlinkHeader, payload: &[u8]) -> Result<()> {
    if header.flags & NLM_F_DUMP_INTR != 0 {
        bail!("SOCK_DIAG dump was interrupted; refusing an incomplete manifest");
    }
    // Modern netlink dumps may carry a native-endian i32 status in
    // NLMSG_DONE, followed by optional extended-ack attributes.  Treating the
    // message type alone as success can publish a partial cookie manifest.
    if payload.is_empty() {
        return Ok(());
    }
    let error = read_unaligned::<i32>(payload)?;
    if error == 0 {
        return Ok(());
    }
    let errno = error
        .checked_neg()
        .filter(|errno| *errno > 0)
        .context("SOCK_DIAG dump completion contained an invalid netlink error")?;
    bail!(
        "SOCK_DIAG dump completion failed with errno {errno} ({}); refusing an incomplete manifest",
        io::Error::from_raw_os_error(errno)
    )
}

fn dump_family(family: u8) -> Result<Vec<TcpSocketIdentity>> {
    let fd = open_diag_socket()?;
    let sequence = NEXT_NETLINK_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let states = ((1u32 << (TCP_STATE_MAX + 1)) - 1) & !(1u32 << TCP_LISTEN);
    let request = InetDiagRequest {
        family,
        protocol: IPPROTO_TCP,
        extensions: 0,
        pad: 0,
        states,
        id: InetDiagSockId {
            source_port_be: 0,
            destination_port_be: 0,
            source: [0; 4],
            destination: [0; 4],
            interface_id: 0,
            cookie: [INET_DIAG_NOCOOKIE; 2],
        },
    };
    send_diag_request(
        &fd,
        SOCK_DIAG_BY_FAMILY,
        NLM_F_REQUEST | NLM_F_DUMP,
        &request,
        sequence,
    )?;
    let mut sockets = Vec::new();
    loop {
        let datagram = receive_datagram(&fd)?;
        let done = for_each_netlink_message(&datagram, sequence, |header, payload| {
            match header.message_type {
                NLMSG_DONE => {
                    validate_dump_completion(header, payload)?;
                    return Ok(true);
                }
                NLMSG_ERROR => {
                    let (error, request) = decode_netlink_error(payload)?;
                    validate_error_request(request, sequence, SOCK_DIAG_BY_FAMILY)?;
                    if error != 0 {
                        let errno = -error;
                        bail!(
                            "SOCK_DIAG dump failed with errno {errno} ({}); guest kernel needs CONFIG_INET_DIAG=y",
                            io::Error::from_raw_os_error(errno)
                        );
                    }
                }
                NLMSG_NOOP => {}
                NLMSG_OVERRUN => bail!("SOCK_DIAG dump overran its receive buffer"),
                SOCK_DIAG_BY_FAMILY => {
                    let message: InetDiagMessage = read_unaligned(payload)?;
                    if message.id.cookie == [INET_DIAG_NOCOOKIE; 2] {
                        bail!("kernel returned a TCP socket without an identity cookie");
                    }
                    sockets.push(TcpSocketIdentity {
                        family: message.family,
                        state: message.state,
                        id: message.id,
                    });
                }
                other => bail!("unexpected SOCK_DIAG dump message type {other}"),
            }
            Ok(false)
        })?;
        if done {
            return Ok(sockets);
        }
    }
}

fn destroy_socket(socket: &TcpSocketIdentity) -> Result<DestroyOutcome> {
    let fd = open_diag_socket()?;
    let sequence = NEXT_NETLINK_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let request = InetDiagRequest {
        family: socket.family,
        protocol: IPPROTO_TCP,
        extensions: 0,
        pad: 0,
        states: 0,
        id: socket.id,
    };
    send_diag_request(
        &fd,
        SOCK_DESTROY,
        NLM_F_REQUEST | NLM_F_ACK,
        &request,
        sequence,
    )?;
    loop {
        let datagram = receive_datagram(&fd)?;
        if let Some(outcome) = parse_destroy_reply(&datagram, sequence)? {
            return Ok(outcome);
        }
    }
}

fn parse_destroy_reply(bytes: &[u8], sequence: u32) -> Result<Option<DestroyOutcome>> {
    let mut outcome = None;
    for_each_netlink_message(bytes, sequence, |header, payload| {
        match header.message_type {
            NLMSG_ERROR => {
                let (error, request) = decode_netlink_error(payload)?;
                validate_error_request(request, sequence, SOCK_DESTROY)?;
                if error == 0 {
                    outcome = Some(DestroyOutcome::Destroyed);
                    return Ok(true);
                }
                let errno = -error;
                if errno == libc::ENOENT || errno == libc::ESTALE {
                    outcome = Some(DestroyOutcome::AlreadyGone);
                    return Ok(true);
                }
                if errno == libc::EOPNOTSUPP || errno == libc::ENOSYS {
                    bail!(
                        "cookie-bound SOCK_DESTROY is unsupported (errno {errno}: {}); \
                         the guest kernel must enable CONFIG_INET_DIAG=y and \
                         CONFIG_INET_DIAG_DESTROY=y. Refusing tuple-only or global \
                         conntrack cleanup",
                        io::Error::from_raw_os_error(errno)
                    );
                }
                bail!(
                    "cookie-bound SOCK_DESTROY failed with errno {errno}: {}",
                    io::Error::from_raw_os_error(errno)
                );
            }
            NLMSG_NOOP => {}
            NLMSG_OVERRUN => bail!("SOCK_DESTROY acknowledgement overran receive buffer"),
            NLMSG_DONE => bail!("SOCK_DESTROY ended without a netlink acknowledgement"),
            other => bail!(
                "SOCK_DESTROY returned message type {other} without NLMSG_ERROR acknowledgement; \
                 displayed socket data is not a destruction oracle"
            ),
        }
        Ok(false)
    })?;
    Ok(outcome)
}

async fn reopen_with<G: FirewallGate, L: ExternalLink, T: RouteTable>(
    gate: &G,
    link: &L,
    routes: &T,
    captured: &[CapturedRoute],
) -> Result<()> {
    // Remove both family-specific hooks while the link is still down, then make
    // the link visible as the final publication step.  Removing IPv4 and IPv6
    // hooks cannot be one iptables transaction; doing it behind link-down
    // prevents a partial gate removal from publishing either family.
    gate.open()
        .await
        .context("opening the snapshot TCP NEW-flow gate while the link is down")?;
    link.up()
        .await
        .context("bringing the external link up after opening the snapshot TCP NEW-flow gate")?;
    // Only now: a route needs its device up, and the kernel has just put back
    // the connected routes these ones depend on.
    routes
        .reinstate(captured)
        .await
        .context("reinstating the routes the link-down purged")
}

async fn rollback_preparation<G: FirewallGate, L: ExternalLink, T: RouteTable>(
    error: anyhow::Error,
    gate: &G,
    link: &L,
    routes: &T,
    captured: &[CapturedRoute],
) -> anyhow::Error {
    match reopen_with(gate, link, routes, captured).await {
        Ok(()) => error,
        Err(reopen) => {
            anyhow::anyhow!("{error:#}; additionally failed to restore source network: {reopen:#}")
        }
    }
}

async fn prepare_with<
    G: FirewallGate,
    L: ExternalLink,
    B: PacketReceiveBarrier,
    D: SocketDiagnostic,
    S: ManifestStore,
    T: RouteTable,
>(
    gate: &G,
    link: &L,
    receive_barrier: &B,
    diagnostic: &mut D,
    store: &S,
    route_table: &T,
) -> Result<usize> {
    // Read the route table while it still exists: the link-down below empties
    // it, and every rollback and restore path from here on has to put it back.
    let captured_routes = match route_table
        .capture()
        .await
        .context("capturing the routes the snapshot boundary is about to purge")
    {
        Ok(routes) => routes,
        // Nothing is torn down yet, so there is nothing to roll back.
        Err(error) => return Err(error),
    };
    if let Err(error) = gate
        .close()
        .await
        .context("closing snapshot TCP NEW-flow gate")
    {
        return Err(rollback_preparation(error, gate, link, route_table, &captured_routes).await);
    }
    if let Err(error) = link
        .down()
        .await
        .context("bringing the external link down before snapshot")
    {
        return Err(rollback_preparation(error, gate, link, route_table, &captured_routes).await);
    }
    if let Err(error) = receive_barrier
        .synchronize()
        .context("waiting for pre-boundary packet receive processing")
    {
        return Err(rollback_preparation(error, gate, link, route_table, &captured_routes).await);
    }
    let prepared = (|| -> Result<Vec<TcpSocketIdentity>> {
        let sockets = diagnostic
            .dump_tcp_sockets()
            .context("capturing cookie-bound TCP identities before snapshot")?;
        sockets
            .into_iter()
            .filter_map(|socket| match socket.is_preserved() {
                Ok(true) => None,
                Ok(false) => Some(Ok(socket)),
                Err(error) => Some(Err(error)),
            })
            .collect()
    })();
    let sockets = match prepared {
        Ok(sockets) => sockets,
        Err(error) => {
            return Err(
                rollback_preparation(error, gate, link, route_table, &captured_routes).await,
            )
        }
    };
    let manifest = SnapshotNetworkManifest {
        version: MANIFEST_VERSION,
        sockets,
        routes: captured_routes.clone(),
    };
    if let Err(error) = store.save(&manifest) {
        return Err(rollback_preparation(error, gate, link, route_table, &captured_routes).await);
    }
    Ok(manifest.sockets.len())
}

async fn restore_with<
    G: FirewallGate,
    L: ExternalLink,
    B: PacketReceiveBarrier,
    D: SocketDiagnostic,
    S: ManifestStore,
    T: RouteTable,
>(
    gate: &G,
    link: &L,
    receive_barrier: &B,
    diagnostic: &mut D,
    store: &S,
    route_table: &T,
    budget: std::time::Duration,
) -> Result<RestoreNetworkReport> {
    // The gate chains are VERIFIED rather than reinstalled; the link-down and
    // the receive barrier are always re-asserted.
    //
    // Verifying the chains is sound because a restored image was captured
    // behind them and reinstalling identical rules changes nothing: the check
    // reads what `close()` would have written and skips the write when they
    // match. That is where the cost was, roughly 25 process spawns.
    //
    // The link and the barrier are not verified, because reading them proves
    // less than asserting them. A `--privileged` workload resumes in this
    // network namespace holding CAP_NET_ADMIN, so between any observation and
    // the socket cleanup below it can raise the link itself; the guest-wide
    // lock this transaction holds excludes other fc-agent transactions, not
    // the workload. Downing the link and taking a fresh grace period costs no
    // process spawn (both are direct syscalls) and restores the property the
    // cleanup depends on: no receive processing is in flight, and none can
    // start, while the manifest's sockets are retired.
    //
    // A workload with CAP_NET_ADMIN can still raise the link again afterwards;
    // nothing inside the guest can prevent that, and it was equally true
    // before this fast path existed. What the re-assert guarantees is that the
    // boundary holds across the cleanup itself.
    //
    // Chain verification that FAILS, or that cannot be read at all, falls back
    // to reinstalling them: a malformed or tampered image cannot publish
    // early, and a transient probe failure must never become a dead clone the
    // spawn-free path would have restored.
    let verify_started = std::time::Instant::now();
    let verified_armed = match gate.is_closed().await {
        Ok(verdict) => verdict,
        Err(error) => {
            eprintln!(
                "[fc-agent] WARNING: boundary gate unreadable, reinstalling \
                 instead: {error:#}"
            );
            false
        }
    };
    let verify_ms = verify_started.elapsed().as_secs_f64() * 1000.0;
    let reassert_started = std::time::Instant::now();
    if !verified_armed {
        gate.close()
            .await
            .context("ensuring restored snapshot TCP NEW-flow gate is closed")?;
    }
    link.down()
        .await
        .context("ensuring the restored external link is down during socket cleanup")?;
    receive_barrier
        .synchronize()
        .context("waiting for restored packet receive processing to quiesce")?;
    let reassert_ms = if verified_armed {
        0.0
    } else {
        reassert_started.elapsed().as_secs_f64() * 1000.0
    };
    let destroy_started = std::time::Instant::now();
    let manifest = store.load()?;
    let started = std::time::Instant::now();
    let mut tally = CleanupTally::default();
    for socket in &manifest.sockets {
        let elapsed = started.elapsed();
        if elapsed >= budget {
            bail!(
                "cookie-bound cleanup exceeded its {:?} budget after {:?} with \
                 {} of {} sockets retired; the clone stays unpublished",
                budget,
                elapsed,
                tally.destroyed + tally.already_gone,
                manifest.sockets.len()
            );
        }
        let outcome = diagnostic.destroy(socket).with_context(|| {
            format!("destroying snapshot-time TCP socket {}", socket.describe())
        })?;
        match outcome {
            DestroyOutcome::Destroyed => tally.destroyed += 1,
            DestroyOutcome::AlreadyGone => tally.already_gone += 1,
        }
    }
    // Retire the armed-boundary marker before publication. Leaving it behind
    // would keep the restore watcher on the restore-only control generation;
    // a removal failure therefore remains closed instead of being a warning.
    store
        .remove()
        .context("retiring snapshot network manifest after cookie-bound cleanup")?;
    let destroy_ms = destroy_started.elapsed().as_secs_f64() * 1000.0;
    let reopen_started = std::time::Instant::now();
    reopen_with(gate, link, route_table, &manifest.routes)
        .await
        .context("publishing restored network after cookie-bound cleanup")?;
    Ok(RestoreNetworkReport {
        tally,
        verified_armed,
        verify_ms,
        reassert_ms,
        destroy_ms,
        reopen_ms: reopen_started.elapsed().as_secs_f64() * 1000.0,
    })
}

pub async fn prepare_snapshot_network() -> Result<()> {
    let _transaction_lock = acquire_boundary_lock()?;
    let gate = IptablesGate::new(SystemCommandRunner);
    let link = IpLink::new(external_interface()?);
    let receive_barrier = SystemPacketReceiveBarrier;
    let mut diagnostic = SystemSocketDiagnostic;
    let route_table = IpRoutes::new(SystemCommandRunner);
    let captured = prepare_with(
        &gate,
        &link,
        &receive_barrier,
        &mut diagnostic,
        &FileManifestStore::default(),
        &route_table,
    )
    .await?;
    eprintln!(
        "[fc-agent] snapshot network boundary prepared: gate=closed link=down \
         external_socket_cookies={captured}"
    );
    Ok(())
}

pub async fn resume_source_network() -> Result<()> {
    let _transaction_lock = acquire_boundary_lock()?;
    let gate = IptablesGate::new(SystemCommandRunner);
    let link = IpLink::new(external_interface()?);
    let route_table = IpRoutes::new(SystemCommandRunner);
    let store = FileManifestStore::default();
    // Read the routes out before retiring the manifest that holds them: this
    // is the source VM resuming after its own snapshot, and its link-down
    // purged the same routes a clone's would.
    let captured = store.load().map(|manifest| manifest.routes);
    let routes = match captured {
        Ok(routes) => routes,
        // The source can be resumed without a manifest (a preparation that
        // failed before saving one). Publishing the link still has to happen;
        // there is simply nothing recorded to reinstate.
        Err(error) => {
            eprintln!(
                "[fc-agent] no snapshot network manifest to read routes from, \
                 reopening without reinstating any: {error:#}"
            );
            Vec::new()
        }
    };
    store
        .remove()
        .context("retiring source snapshot network manifest before publication")?;
    reopen_with(&gate, &link, &route_table, &routes)
        .await
        .context("reopening source VM network")?;
    eprintln!("[fc-agent] source VM snapshot network reopened: link=up gate=open");
    Ok(())
}

pub async fn restore_snapshot_network() -> Result<RestoreNetworkReport> {
    let _transaction_lock = acquire_boundary_lock()?;
    let gate = IptablesGate::new(SystemCommandRunner);
    let link = IpLink::new(external_interface()?);
    let receive_barrier = SystemPacketReceiveBarrier;
    let mut diagnostic = SystemSocketDiagnostic;
    let route_table = IpRoutes::new(SystemCommandRunner);
    let report = restore_with(
        &gate,
        &link,
        &receive_barrier,
        &mut diagnostic,
        &FileManifestStore::default(),
        &route_table,
        CLEANUP_BUDGET,
    )
    .await?;
    eprintln!(
        "[fc-agent] snapshot network cleanup complete: destroyed={} already_gone={} \
         verified_armed={} link=up gate=open",
        report.tally.destroyed, report.tally.already_gone, report.verified_armed
    );
    Ok(report)
}

/// Whether this process image was captured behind the snapshot network
/// boundary.  Firecracker normally obtains restore metadata through MMDS on
/// its external interface, but a current snapshot deliberately captures that
/// link down.  The restore watcher uses this marker to select the host's
/// restore-only vsock control plane before touching MMDS.
pub fn boundary_is_armed() -> bool {
    Path::new(MANIFEST_PATH).exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    use std::sync::{Arc, Mutex};

    fn socket(cookie: u32, peer: [u8; 4]) -> TcpSocketIdentity {
        TcpSocketIdentity {
            family: libc::AF_INET as u8,
            state: 1,
            id: InetDiagSockId {
                source_port_be: 49152u16.to_be(),
                destination_port_be: 443u16.to_be(),
                source: [u32::from_ne_bytes([10, 0, 2, 100]), 0, 0, 0],
                destination: [u32::from_ne_bytes(peer), 0, 0, 0],
                interface_id: 0,
                cookie: [cookie, 0],
            },
        }
    }

    #[derive(Default)]
    struct MemoryStore(Mutex<Option<SnapshotNetworkManifest>>);

    impl ManifestStore for MemoryStore {
        fn save(&self, manifest: &SnapshotNetworkManifest) -> Result<()> {
            *self.0.lock().unwrap() = Some(manifest.clone());
            Ok(())
        }

        fn load(&self) -> Result<SnapshotNetworkManifest> {
            self.0.lock().unwrap().clone().context("manifest missing")
        }

        fn remove(&self) -> Result<()> {
            *self.0.lock().unwrap() = None;
            Ok(())
        }
    }

    struct RejectingRemoveStore(MemoryStore);

    impl ManifestStore for RejectingRemoveStore {
        fn save(&self, manifest: &SnapshotNetworkManifest) -> Result<()> {
            self.0.save(manifest)
        }

        fn load(&self) -> Result<SnapshotNetworkManifest> {
            self.0.load()
        }

        fn remove(&self) -> Result<()> {
            bail!("injected manifest retirement failure")
        }
    }

    #[derive(Default)]
    struct ModelState {
        gate_closed: bool,
        link_down: bool,
        /// The guest's live route table, as the kernel keeps it: emptied when
        /// the link goes down and NOT refilled when it comes back up. Without
        /// modelling that, a test cannot tell a boundary that reinstates routes
        /// from one that silently drops them.
        live_routes: Vec<CapturedRoute>,
        events: Vec<&'static str>,
        live: Vec<TcpSocketIdentity>,
        fail_gate_close: bool,
        fail_gate_verify: bool,
        fail_barrier: bool,
        fail_dump: bool,
        fail_destroy: bool,
    }

    #[derive(Clone)]
    struct ModelGate(Arc<Mutex<ModelState>>);

    impl FirewallGate for ModelGate {
        async fn close(&self) -> Result<()> {
            let mut state = self.0.lock().unwrap();
            state.gate_closed = true;
            state.events.push("gate-close");
            if state.fail_gate_close {
                bail!("injected gate close failure after partial installation");
            }
            Ok(())
        }

        async fn open(&self) -> Result<()> {
            let mut state = self.0.lock().unwrap();
            state.gate_closed = false;
            state.events.push("gate-open");
            Ok(())
        }

        async fn is_closed(&self) -> Result<bool> {
            let mut state = self.0.lock().unwrap();
            state.events.push("gate-verify");
            if state.fail_gate_verify {
                bail!("injected gate verification read failure");
            }
            Ok(state.gate_closed)
        }
    }

    #[derive(Clone)]
    struct ModelLink(Arc<Mutex<ModelState>>);

    impl ExternalLink for ModelLink {
        async fn down(&self) -> Result<()> {
            let mut state = self.0.lock().unwrap();
            state.link_down = true;
            // Measured on a bridged guest: the whole table goes with the link.
            state.live_routes.clear();
            state.events.push("link-down");
            Ok(())
        }

        async fn up(&self) -> Result<()> {
            let mut state = self.0.lock().unwrap();
            assert!(
                !state.gate_closed,
                "link publication raced ahead of complete gate removal"
            );
            state.link_down = false;
            state.events.push("link-up");
            Ok(())
        }
    }

    #[derive(Clone)]
    struct ModelRoutes(Arc<Mutex<ModelState>>);

    impl RouteTable for ModelRoutes {
        async fn capture(&self) -> Result<Vec<CapturedRoute>> {
            let mut state = self.0.lock().unwrap();
            assert!(
                !state.link_down,
                "routes were captured after the link went down, when the table is already empty"
            );
            state.events.push("routes-capture");
            Ok(state.live_routes.clone())
        }

        async fn reinstate(&self, routes: &[CapturedRoute]) -> Result<()> {
            let mut state = self.0.lock().unwrap();
            assert!(
                !state.link_down,
                "routes were reinstated while the link was still down, where they cannot bind"
            );
            state.events.push("routes-reinstate");
            for route in routes {
                if !state.live_routes.contains(route) {
                    state.live_routes.push(route.clone());
                }
            }
            Ok(())
        }
    }

    fn model_routes() -> Vec<CapturedRoute> {
        vec![
            CapturedRoute {
                family: 4,
                spec: "default via 172.30.95.37 dev eth0".to_string(),
            },
            CapturedRoute {
                family: 4,
                spec: "169.254.169.254 dev eth0 proto static scope link".to_string(),
            },
        ]
    }

    struct ModelBarrier(Arc<Mutex<ModelState>>);

    impl PacketReceiveBarrier for ModelBarrier {
        fn synchronize(&self) -> Result<()> {
            let mut state = self.0.lock().unwrap();
            assert!(
                state.gate_closed,
                "receive barrier ran before the NEW-flow gate"
            );
            assert!(
                state.link_down,
                "receive barrier ran before the link was down"
            );
            state.events.push("rx-barrier");
            if state.fail_barrier {
                bail!("injected receive barrier failure");
            }
            Ok(())
        }
    }

    struct ModelDiag(Arc<Mutex<ModelState>>);

    impl SocketDiagnostic for ModelDiag {
        fn dump_tcp_sockets(&mut self) -> Result<Vec<TcpSocketIdentity>> {
            let mut state = self.0.lock().unwrap();
            assert!(
                state.gate_closed,
                "cookie dump raced ahead of the NEW-flow gate"
            );
            assert!(state.link_down, "cookie dump raced ahead of link-down");
            state.events.push("cookie-dump");
            if state.fail_dump {
                bail!("injected socket dump failure");
            }
            Ok(state.live.clone())
        }

        fn destroy(&mut self, socket: &TcpSocketIdentity) -> Result<DestroyOutcome> {
            let mut state = self.0.lock().unwrap();
            assert!(
                state.gate_closed,
                "restore opened the gate before destroying sockets"
            );
            assert!(
                state.link_down,
                "restore brought the link up before destroying sockets"
            );
            state.events.push("cookie-destroy");
            if state.fail_destroy {
                bail!("injected cookie destroy failure");
            }
            if let Some(index) = state.live.iter().position(|candidate| candidate == socket) {
                state.live.remove(index);
                Ok(DestroyOutcome::Destroyed)
            } else {
                Ok(DestroyOutcome::AlreadyGone)
            }
        }
    }

    #[derive(Clone, Default)]
    struct RecordingRunner(Arc<Mutex<Vec<String>>>);

    impl CommandRunner for RecordingRunner {
        async fn output(&self, program: &str, args: &[&str]) -> io::Result<std::process::Output> {
            self.0
                .lock()
                .unwrap()
                .push(format!("{program} {}", args.join(" ")));
            Ok(std::process::Output {
                status: std::process::ExitStatus::from_raw(0),
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }

        async fn output_with_input(
            &self,
            program: &str,
            args: &[&str],
            input: &[u8],
        ) -> io::Result<std::process::Output> {
            self.0.lock().unwrap().push(format!(
                "{program} {} <<< {:?}",
                args.join(" "),
                String::from_utf8_lossy(input)
            ));
            Ok(std::process::Output {
                status: std::process::ExitStatus::from_raw(0),
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }

    #[tokio::test]
    async fn gateway_address_cannot_bypass_directional_new_flow_gate() {
        let runner = RecordingRunner::default();
        let calls = runner.0.clone();
        IptablesGate::new(runner).close().await.unwrap();
        let calls = calls.lock().unwrap().join("\n");

        assert!(calls.contains("iptables -w -A FCVM_SNAPSHOT_IN -p tcp -s 127.0.0.0/8 -j RETURN"));
        assert!(calls.contains("iptables -w -A FCVM_SNAPSHOT_OUT -p tcp -d 127.0.0.0/8 -j RETURN"));
        assert!(calls.contains(
            "iptables -w -A FCVM_SNAPSHOT_IN -p tcp -m conntrack --ctstate NEW -j REJECT"
        ));
        assert!(calls.contains(
            "iptables -w -A FCVM_SNAPSHOT_OUT -p tcp -m conntrack --ctstate NEW -j REJECT"
        ));
        assert!(
            !calls.contains("10.0.2.") && !calls.contains("fd00:"),
            "gateway exemptions let routed external clients cross the generation boundary:\n{calls}"
        );
    }

    /// Runner that answers `-S` with a canned dump and records every call.
    #[derive(Clone)]
    struct ScriptedRunner {
        dump: String,
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl CommandRunner for ScriptedRunner {
        async fn output(&self, program: &str, args: &[&str]) -> io::Result<std::process::Output> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("{program} {}", args.join(" ")));
            let stdout = if args == ["-w", "-S"] {
                self.dump.clone().into_bytes()
            } else {
                Vec::new()
            };
            Ok(std::process::Output {
                status: std::process::ExitStatus::from_raw(0),
                stdout,
                stderr: Vec::new(),
            })
        }

        async fn output_with_input(
            &self,
            program: &str,
            args: &[&str],
            input: &[u8],
        ) -> io::Result<std::process::Output> {
            self.calls.lock().unwrap().push(format!(
                "{program} {} <<< {:?}",
                args.join(" "),
                String::from_utf8_lossy(input)
            ));
            Ok(std::process::Output {
                status: std::process::ExitStatus::from_raw(0),
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }

    /// Verbatim `iptables -w -S` output of an armed IPv4 gate, captured on
    /// iptables v1.8.10 (nf_tables), Ubuntu 24.04 aarch64. Note the renderer
    /// prints `-s`/`-d` BEFORE `-p tcp`, the reverse of the appended argument
    /// order; verification must accept the renderer's form.
    const ARMED_V4_DUMP: &str = "-P INPUT ACCEPT\n\
        -P FORWARD ACCEPT\n\
        -P OUTPUT ACCEPT\n\
        -N FCVM_SNAPSHOT_IN\n\
        -N FCVM_SNAPSHOT_OUT\n\
        -A INPUT -j FCVM_SNAPSHOT_IN\n\
        -A OUTPUT -j FCVM_SNAPSHOT_OUT\n\
        -A FCVM_SNAPSHOT_IN -s 127.0.0.0/8 -p tcp -j RETURN\n\
        -A FCVM_SNAPSHOT_IN -p tcp -m conntrack --ctstate NEW -j REJECT --reject-with tcp-reset\n\
        -A FCVM_SNAPSHOT_IN -j RETURN\n\
        -A FCVM_SNAPSHOT_OUT -d 127.0.0.0/8 -p tcp -j RETURN\n\
        -A FCVM_SNAPSHOT_OUT -p tcp -m conntrack --ctstate NEW -j REJECT --reject-with tcp-reset\n\
        -A FCVM_SNAPSHOT_OUT -j RETURN\n";

    /// Same capture for the IPv6 family (`-d ::1/128 -p tcp -j RETURN` etc.).
    const ARMED_V6_DUMP: &str = "-P INPUT ACCEPT\n\
        -P FORWARD ACCEPT\n\
        -P OUTPUT ACCEPT\n\
        -N FCVM_SNAPSHOT_IN\n\
        -N FCVM_SNAPSHOT_OUT\n\
        -A INPUT -j FCVM_SNAPSHOT_IN\n\
        -A OUTPUT -j FCVM_SNAPSHOT_OUT\n\
        -A FCVM_SNAPSHOT_IN -s ::1/128 -p tcp -j RETURN\n\
        -A FCVM_SNAPSHOT_IN -p tcp -m conntrack --ctstate NEW -j REJECT --reject-with tcp-reset\n\
        -A FCVM_SNAPSHOT_IN -j RETURN\n\
        -A FCVM_SNAPSHOT_OUT -d ::1/128 -p tcp -j RETURN\n\
        -A FCVM_SNAPSHOT_OUT -p tcp -m conntrack --ctstate NEW -j REJECT --reject-with tcp-reset\n\
        -A FCVM_SNAPSHOT_OUT -j RETURN\n";

    #[test]
    fn armed_gate_verifies_from_the_live_iptables_rendering() {
        assert!(family_gate_is_closed(
            &parse_gate_state(ARMED_V4_DUMP),
            IPV4_LOOPBACK
        ));
        assert!(family_gate_is_closed(
            &parse_gate_state(ARMED_V6_DUMP),
            IPV6_LOOPBACK
        ));
    }

    #[test]
    fn any_deviation_from_the_armed_gate_fails_verification() {
        // Missing hook: the chain exists but nothing routes packets into it.
        let unhooked = ARMED_V4_DUMP.replace("-A INPUT -j FCVM_SNAPSHOT_IN\n", "");
        assert!(!family_gate_is_closed(
            &parse_gate_state(&unhooked),
            IPV4_LOOPBACK
        ));

        // A foreign rule inside the gate chain is not our gate.
        let extra_rule = format!("{ARMED_V4_DUMP}-A FCVM_SNAPSHOT_IN -p udp -j ACCEPT\n");
        assert!(!family_gate_is_closed(
            &parse_gate_state(&extra_rule),
            IPV4_LOOPBACK
        ));

        // A pristine table has no boundary at all.
        assert!(!family_gate_is_closed(
            &parse_gate_state("-P INPUT ACCEPT\n-P FORWARD ACCEPT\n-P OUTPUT ACCEPT\n"),
            IPV4_LOOPBACK
        ));

        // The wrong loopback exemption (v6 rules under the v4 expectation)
        // must not verify.
        assert!(!family_gate_is_closed(
            &parse_gate_state(ARMED_V6_DUMP),
            IPV4_LOOPBACK
        ));
    }

    #[test]
    fn a_rule_ahead_of_the_gate_hook_fails_verification() {
        // The gate only governs packets that reach it. An ACCEPT sitting
        // above the jump admits NEW flows the manifest never captured, so a
        // hook anywhere but position 1 must force the full re-assert path.
        let shadowed = ARMED_V4_DUMP.replace(
            "-A INPUT -j FCVM_SNAPSHOT_IN\n",
            "-A INPUT -p tcp -j ACCEPT\n-A INPUT -j FCVM_SNAPSHOT_IN\n",
        );
        assert!(!family_gate_is_closed(
            &parse_gate_state(&shadowed),
            IPV4_LOOPBACK
        ));

        let shadowed_output = ARMED_V4_DUMP.replace(
            "-A OUTPUT -j FCVM_SNAPSHOT_OUT\n",
            "-A OUTPUT -p tcp -j ACCEPT\n-A OUTPUT -j FCVM_SNAPSHOT_OUT\n",
        );
        assert!(!family_gate_is_closed(
            &parse_gate_state(&shadowed_output),
            IPV4_LOOPBACK
        ));
    }

    #[test]
    fn duplicate_hooks_still_verify_as_closed() {
        // An interrupted close can leave a second jump; the gate verdict for
        // packets is unchanged, so verification must not force a repair.
        let doubled = format!("{ARMED_V4_DUMP}-A INPUT -j FCVM_SNAPSHOT_IN\n");
        assert!(family_gate_is_closed(
            &parse_gate_state(&doubled),
            IPV4_LOOPBACK
        ));
    }

    #[tokio::test]
    async fn computed_removal_issues_exactly_the_observed_deletions() {
        let doubled = format!("{ARMED_V4_DUMP}-A INPUT -j FCVM_SNAPSHOT_IN\n");
        let runner = ScriptedRunner {
            dump: doubled,
            calls: Arc::new(Mutex::new(Vec::new())),
        };
        let calls = runner.calls.clone();
        IptablesGate::new(runner)
            .remove_family("iptables")
            .await
            .unwrap();
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            [
                "iptables -w -S",
                "iptables-restore -w --noflush <<< \"*filter\\n\
-D INPUT -j FCVM_SNAPSHOT_IN\\n\
-D INPUT -j FCVM_SNAPSHOT_IN\\n\
-D OUTPUT -j FCVM_SNAPSHOT_OUT\\n\
-F FCVM_SNAPSHOT_IN\\n\
-X FCVM_SNAPSHOT_IN\\n\
-F FCVM_SNAPSHOT_OUT\\n\
-X FCVM_SNAPSHOT_OUT\\n\
COMMIT\\n\"",
            ],
            "removal must be one computed batch, never a probe or spawn per step"
        );
    }

    #[tokio::test]
    async fn removal_reuses_the_verification_read_in_one_transaction() {
        let runner = ScriptedRunner {
            dump: ARMED_V4_DUMP.to_string(),
            calls: Arc::new(Mutex::new(Vec::new())),
        };
        let calls = runner.calls.clone();
        let gate = IptablesGate::new(runner);
        assert!(gate
            .family_is_closed("iptables", IPV4_LOOPBACK)
            .await
            .unwrap());
        gate.remove_family("iptables").await.unwrap();
        let reads = calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.as_str() == "iptables -w -S")
            .count();
        assert_eq!(
            reads, 1,
            "verify-then-remove under one lock must spend one state read"
        );
    }

    #[tokio::test]
    async fn a_gate_mutation_invalidates_the_retained_state_read() {
        let runner = ScriptedRunner {
            dump: ARMED_V4_DUMP.to_string(),
            calls: Arc::new(Mutex::new(Vec::new())),
        };
        let calls = runner.calls.clone();
        let gate = IptablesGate::new(runner);
        assert!(gate
            .family_is_closed("iptables", IPV4_LOOPBACK)
            .await
            .unwrap());
        // The repair path re-closes after a failed verification; the removal
        // that follows must observe the post-close state, not the retained
        // pre-close parse.
        gate.install_family("iptables", IPV4_LOOPBACK)
            .await
            .unwrap();
        gate.remove_family("iptables").await.unwrap();
        let reads = calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.as_str() == "iptables -w -S")
            .count();
        assert_eq!(
            reads, 2,
            "a close between verify and remove must force a fresh read"
        );
    }

    #[tokio::test]
    async fn removal_of_an_absent_gate_is_a_read_and_nothing_else() {
        let runner = ScriptedRunner {
            dump: "-P INPUT ACCEPT\n-P FORWARD ACCEPT\n-P OUTPUT ACCEPT\n".to_string(),
            calls: Arc::new(Mutex::new(Vec::new())),
        };
        let calls = runner.calls.clone();
        IptablesGate::new(runner)
            .remove_family("iptables")
            .await
            .unwrap();
        assert_eq!(calls.lock().unwrap().as_slice(), ["iptables -w -S"]);
    }

    /// Build a `/sys/class/net` where the `backed` interfaces carry the
    /// `device` symlink a bus-attached netdev has and the others do not.
    fn fake_sysfs_net(interfaces: &[(&str, bool)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let devices = dir.path().join("devices");
        std::fs::create_dir_all(&devices).unwrap();
        for (name, backed) in interfaces {
            let interface = dir.path().join(name);
            std::fs::create_dir_all(&interface).unwrap();
            if *backed {
                let device = devices.join(name);
                std::fs::create_dir_all(&device).unwrap();
                std::os::unix::fs::symlink(&device, interface.join("device")).unwrap();
            }
        }
        dir
    }

    #[test]
    fn external_interface_is_the_device_backed_netdev_under_either_hypervisor() {
        // Measured in live guests: Cloud Hypervisor's PCI virtio-net is renamed
        // to enp0s4, Firecracker's MMIO virtio-net keeps eth0, and on both only
        // `lo` lacks the device symlink.
        let cloud_hypervisor = fake_sysfs_net(&[("enp0s4", true), ("lo", false)]);
        assert_eq!(
            external_interface_in(cloud_hypervisor.path()).unwrap(),
            "enp0s4"
        );

        let firecracker = fake_sysfs_net(&[("eth0", true), ("lo", false)]);
        assert_eq!(external_interface_in(firecracker.path()).unwrap(), "eth0");
    }

    #[test]
    fn an_ambiguous_interface_set_fails_instead_of_guessing() {
        let loopback_only = fake_sysfs_net(&[("lo", false)]);
        let err = external_interface_in(loopback_only.path())
            .unwrap_err()
            .to_string();
        assert!(err.contains("found []"), "{err}");

        let two = fake_sysfs_net(&[("enp0s4", true), ("eth0", true), ("lo", false)]);
        let err = external_interface_in(two.path()).unwrap_err().to_string();
        assert!(err.contains("enp0s4") && err.contains("eth0"), "{err}");
    }

    #[test]
    fn an_overlong_interface_name_is_refused_rather_than_truncated() {
        // ifreq's name field is fixed-size; a silently truncated name would
        // drive the WRONG interface's flags.
        let too_long = "x".repeat(64);
        let error = set_interface_up(&too_long, false)
            .expect_err("an interface name that cannot fit must be refused");
        assert!(
            format!("{error:#}").contains("does not fit"),
            "unexpected diagnostic: {error:#}"
        );
    }

    #[tokio::test]
    async fn snapshot_boundary_closes_new_flows_before_cookie_dump() {
        let state = Arc::new(Mutex::new(ModelState {
            live: vec![socket(11, [198, 51, 100, 8])],
            ..Default::default()
        }));
        let gate = ModelGate(state.clone());
        let link = ModelLink(state.clone());
        let barrier = ModelBarrier(state.clone());
        let route_table = ModelRoutes(state.clone());
        state.lock().unwrap().live_routes = model_routes();
        let mut diag = ModelDiag(state.clone());
        let store = MemoryStore::default();

        assert_eq!(
            prepare_with(&gate, &link, &barrier, &mut diag, &store, &route_table)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            state.lock().unwrap().events,
            vec![
                "routes-capture",
                "gate-close",
                "link-down",
                "rx-barrier",
                "cookie-dump"
            ]
        );
        let state_guard = state.lock().unwrap();
        assert!(state_guard.gate_closed);
        assert!(state_guard.link_down);
        drop(state_guard);
        assert_eq!(store.load().unwrap().sockets[0].id.cookie, [11, 0]);
    }

    /// The boundary must hand back the route table it took away.
    ///
    /// Taking the link down purges every route; bringing it up restores only the
    /// kernel's own. Measured on a bridged guest (Graviton3, 6.18.44), a
    /// prepare/resume cycle left `default via 172.30.95.37` and the MMDS route
    /// `169.254.169.254` permanently gone — the guest's only path off-box and
    /// the address fc-agent reads its restore epoch from. Before the fix, this
    /// observed an empty table after the cycle.
    #[tokio::test]
    async fn the_boundary_reinstates_the_routes_its_link_down_purged() {
        let state = Arc::new(Mutex::new(ModelState::default()));
        let gate = ModelGate(state.clone());
        let link = ModelLink(state.clone());
        let barrier = ModelBarrier(state.clone());
        let route_table = ModelRoutes(state.clone());
        state.lock().unwrap().live_routes = model_routes();
        let mut diag = ModelDiag(state.clone());
        let store = MemoryStore::default();

        prepare_with(&gate, &link, &barrier, &mut diag, &store, &route_table)
            .await
            .expect("preparation");
        assert!(
            state.lock().unwrap().live_routes.is_empty(),
            "the model must reproduce the kernel's purge, or this test cannot fail"
        );
        assert_eq!(
            store.load().unwrap().routes,
            model_routes(),
            "the manifest must carry the routes across the snapshot"
        );

        restore_with(
            &gate,
            &link,
            &barrier,
            &mut diag,
            &store,
            &route_table,
            CLEANUP_BUDGET,
        )
        .await
        .expect("restore");
        assert_eq!(
            state.lock().unwrap().live_routes,
            model_routes(),
            "a restored clone was published without the routes it needs to reach anything"
        );

        let events = state.lock().unwrap().events.clone();
        let reinstate = events
            .iter()
            .rposition(|event| *event == "routes-reinstate")
            .expect("routes were never reinstated");
        let link_up = events
            .iter()
            .rposition(|event| *event == "link-up")
            .expect("the link was never published");
        assert!(
            link_up < reinstate,
            "routes were reinstated before the link came up, where they cannot bind: {events:?}"
        );
    }

    #[tokio::test]
    async fn snapshot_boundary_captures_gateway_peers_used_by_routed_ingress() {
        let state = Arc::new(Mutex::new(ModelState {
            // The routed host TCP proxy connects to the guest from 10.0.2.2,
            // exactly like a guest-initiated NFS connection. Direction cannot
            // be recovered from the peer address, so every non-loopback socket
            // must cross the cookie-bound generation cleanup.
            live: vec![socket(12, [10, 0, 2, 2])],
            ..Default::default()
        }));
        let gate = ModelGate(state.clone());
        let link = ModelLink(state.clone());
        let barrier = ModelBarrier(state.clone());
        let route_table = ModelRoutes(state.clone());
        state.lock().unwrap().live_routes = model_routes();
        let mut diag = ModelDiag(state);
        let store = MemoryStore::default();

        assert_eq!(
            prepare_with(&gate, &link, &barrier, &mut diag, &store, &route_table)
                .await
                .unwrap(),
            1
        );
        assert_eq!(store.load().unwrap().sockets[0].id.cookie, [12, 0]);
    }

    #[tokio::test]
    async fn failed_cookie_capture_reopens_the_source_gate() {
        let state = Arc::new(Mutex::new(ModelState {
            fail_dump: true,
            ..Default::default()
        }));
        let gate = ModelGate(state.clone());
        let link = ModelLink(state.clone());
        let barrier = ModelBarrier(state.clone());
        let route_table = ModelRoutes(state.clone());
        state.lock().unwrap().live_routes = model_routes();
        let mut diag = ModelDiag(state.clone());
        let store = MemoryStore::default();

        prepare_with(&gate, &link, &barrier, &mut diag, &store, &route_table)
            .await
            .expect_err("snapshot preparation must fail when cookie capture fails");
        let state = state.lock().unwrap();
        assert_eq!(
            state.events,
            vec![
                "routes-capture",
                "gate-close",
                "link-down",
                "rx-barrier",
                "cookie-dump",
                "gate-open",
                "link-up",
                "routes-reinstate"
            ]
        );
        assert!(
            !state.gate_closed,
            "failed prepare left the source unpublished"
        );
        assert!(!state.link_down, "failed prepare left the source link down");
    }

    #[tokio::test]
    async fn failed_receive_barrier_reopens_source_without_capturing_cookies() {
        let state = Arc::new(Mutex::new(ModelState {
            fail_barrier: true,
            ..Default::default()
        }));
        let gate = ModelGate(state.clone());
        let link = ModelLink(state.clone());
        let barrier = ModelBarrier(state.clone());
        let route_table = ModelRoutes(state.clone());
        state.lock().unwrap().live_routes = model_routes();
        let mut diag = ModelDiag(state.clone());
        let store = MemoryStore::default();

        prepare_with(&gate, &link, &barrier, &mut diag, &store, &route_table)
            .await
            .expect_err("snapshot preparation must fail without a receive grace period");
        let state = state.lock().unwrap();
        assert_eq!(
            state.events,
            vec![
                "routes-capture",
                "gate-close",
                "link-down",
                "rx-barrier",
                "gate-open",
                "link-up",
                "routes-reinstate"
            ]
        );
        assert!(!state.gate_closed);
        assert!(!state.link_down);
        assert!(
            store.load().is_err(),
            "failed boundary published a manifest"
        );
    }

    #[tokio::test]
    async fn partial_gate_close_failure_recovers_the_source_before_returning() {
        let state = Arc::new(Mutex::new(ModelState {
            fail_gate_close: true,
            ..Default::default()
        }));
        let gate = ModelGate(state.clone());
        let link = ModelLink(state.clone());
        let barrier = ModelBarrier(state.clone());
        let route_table = ModelRoutes(state.clone());
        state.lock().unwrap().live_routes = model_routes();
        let mut diag = ModelDiag(state.clone());
        let store = MemoryStore::default();

        prepare_with(&gate, &link, &barrier, &mut diag, &store, &route_table)
            .await
            .expect_err("partial gate installation must abort snapshot preparation");
        let state = state.lock().unwrap();
        assert_eq!(
            state.events,
            vec![
                "routes-capture",
                "gate-close",
                "gate-open",
                "link-up",
                "routes-reinstate"
            ]
        );
        assert!(!state.gate_closed);
        assert!(!state.link_down);
    }

    #[tokio::test]
    async fn cookie_bound_restore_never_kills_tuple_aba_replacement() {
        let stale = socket(21, [198, 51, 100, 8]);
        let replacement = socket(22, [198, 51, 100, 8]);
        let state = Arc::new(Mutex::new(ModelState {
            gate_closed: true,
            link_down: true,
            live: vec![replacement],
            ..Default::default()
        }));
        let store = MemoryStore::default();
        store
            .save(&SnapshotNetworkManifest {
                version: MANIFEST_VERSION,
                routes: model_routes(),
                sockets: vec![stale],
            })
            .unwrap();
        let gate = ModelGate(state.clone());
        let link = ModelLink(state.clone());
        let barrier = ModelBarrier(state.clone());
        let route_table = ModelRoutes(state.clone());
        state.lock().unwrap().live_routes = model_routes();
        let mut diag = ModelDiag(state.clone());

        // The manifest names the stale socket, which the kernel has already
        // retired, and the live table holds only its tuple-identical
        // replacement. A cookie-bound destroy must therefore report
        // already_gone and leave the replacement alone; a tuple-based one would
        // report a destroy and take the wrong socket with it.
        let report = restore_with(
            &gate,
            &link,
            &barrier,
            &mut diag,
            &store,
            &route_table,
            CLEANUP_BUDGET,
        )
        .await
        .unwrap();
        assert_eq!(
            report.tally,
            CleanupTally {
                destroyed: 0,
                already_gone: 1,
            }
        );
        assert!(report.verified_armed);
        let state = state.lock().unwrap();
        assert_eq!(state.live, vec![replacement]);
        assert_eq!(
            state.events,
            vec![
                "gate-verify",
                "link-down",
                "rx-barrier",
                "cookie-destroy",
                "gate-open",
                "link-up",
                "routes-reinstate"
            ]
        );
        assert!(!state.gate_closed);
        assert!(!state.link_down);
    }

    /// A gate probe that cannot READ the chains is not a verdict about them:
    /// it must reinstall, exactly as if verification had failed, never kill
    /// an otherwise healthy clone. The spawn-free path has no reads to fail,
    /// so a read error failing the restore would be a new fatality this
    /// optimization introduced.
    #[tokio::test]
    async fn a_failed_verification_read_demotes_to_the_full_reassert() {
        let stale = socket(28, [198, 51, 100, 8]);
        let state = Arc::new(Mutex::new(ModelState {
            gate_closed: true,
            link_down: true,
            live: vec![stale],
            fail_gate_verify: true,
            ..Default::default()
        }));
        let store = MemoryStore::default();
        store
            .save(&SnapshotNetworkManifest {
                version: MANIFEST_VERSION,
                routes: model_routes(),
                sockets: vec![stale],
            })
            .unwrap();
        let gate = ModelGate(state.clone());
        let link = ModelLink(state.clone());
        let barrier = ModelBarrier(state.clone());
        let route_table = ModelRoutes(state.clone());
        let mut diag = ModelDiag(state.clone());

        let report = restore_with(
            &gate,
            &link,
            &barrier,
            &mut diag,
            &store,
            &route_table,
            CLEANUP_BUDGET,
        )
        .await
        .expect("an unverifiable boundary must repair, not fail the restore");
        assert!(!report.verified_armed);
        let state = state.lock().unwrap();
        assert_eq!(
            state.events,
            vec![
                "gate-verify",
                "gate-close",
                "link-down",
                "rx-barrier",
                "cookie-destroy",
                "gate-open",
                "link-up",
                "routes-reinstate"
            ]
        );
    }

    /// A raised link is exactly what a privileged workload can produce, and
    /// packets can arrive on it: the boundary must down it and take a fresh
    /// grace period before retiring any socket, whatever the gate says.
    #[tokio::test]
    async fn a_raised_link_is_downed_and_re_barriered_before_cleanup() {
        let stale = socket(29, [198, 51, 100, 8]);
        let state = Arc::new(Mutex::new(ModelState {
            gate_closed: true,
            link_down: false,
            live: vec![stale],
            ..Default::default()
        }));
        let store = MemoryStore::default();
        store
            .save(&SnapshotNetworkManifest {
                version: MANIFEST_VERSION,
                routes: model_routes(),
                sockets: vec![stale],
            })
            .unwrap();
        let gate = ModelGate(state.clone());
        let link = ModelLink(state.clone());
        let barrier = ModelBarrier(state.clone());
        let route_table = ModelRoutes(state.clone());
        let mut diag = ModelDiag(state.clone());

        let report = restore_with(
            &gate,
            &link,
            &barrier,
            &mut diag,
            &store,
            &route_table,
            CLEANUP_BUDGET,
        )
        .await
        .unwrap();
        assert!(report.verified_armed, "the gate itself was intact");
        let events = state.lock().unwrap().events.clone();
        let barrier = events
            .iter()
            .position(|e| *e == "rx-barrier")
            .expect("barrier");
        let down = events
            .iter()
            .position(|e| *e == "link-down")
            .expect("link-down");
        let destroy = events
            .iter()
            .position(|e| *e == "cookie-destroy")
            .expect("cleanup");
        assert!(
            down < barrier && barrier < destroy,
            "a raised link must be downed and re-barriered BEFORE cleanup: {events:?}"
        );
    }

    /// The gate chains frozen into a current snapshot are verified, not
    /// reinstalled. The link-down and the receive barrier are asserted
    /// regardless, because a privileged workload sharing this namespace can
    /// raise the link between any observation and the cleanup.
    #[tokio::test]
    async fn restore_verifies_the_armed_boundary_instead_of_reasserting_it() {
        let stale = socket(26, [198, 51, 100, 8]);
        let state = Arc::new(Mutex::new(ModelState {
            gate_closed: true,
            link_down: true,
            live: vec![stale],
            ..Default::default()
        }));
        let store = MemoryStore::default();
        store
            .save(&SnapshotNetworkManifest {
                version: MANIFEST_VERSION,
                routes: model_routes(),
                sockets: vec![stale],
            })
            .unwrap();
        let gate = ModelGate(state.clone());
        let link = ModelLink(state.clone());
        let barrier = ModelBarrier(state.clone());
        let route_table = ModelRoutes(state.clone());
        let mut diag = ModelDiag(state.clone());

        let report = restore_with(
            &gate,
            &link,
            &barrier,
            &mut diag,
            &store,
            &route_table,
            CLEANUP_BUDGET,
        )
        .await
        .unwrap();
        assert!(report.verified_armed);
        let state = state.lock().unwrap();
        assert!(
            !state.events.contains(&"gate-close"),
            "a verified gate was reinstalled: {:?}",
            state.events
        );
        assert_eq!(
            state.events,
            vec![
                "gate-verify",
                "link-down",
                "rx-barrier",
                "cookie-destroy",
                "gate-open",
                "link-up",
                "routes-reinstate"
            ],
            "the link and the barrier must be re-asserted even on the fast path"
        );
    }

    /// A snapshot whose boundary does not verify (here: gate open and link up
    /// under a live manifest) must get the full re-assert, receive barrier
    /// included, before any socket is touched.
    #[tokio::test]
    async fn restore_repairs_an_unverified_boundary_before_cleanup() {
        let stale = socket(27, [198, 51, 100, 8]);
        let state = Arc::new(Mutex::new(ModelState {
            live: vec![stale],
            ..Default::default()
        }));
        let store = MemoryStore::default();
        store
            .save(&SnapshotNetworkManifest {
                version: MANIFEST_VERSION,
                routes: model_routes(),
                sockets: vec![stale],
            })
            .unwrap();
        let gate = ModelGate(state.clone());
        let link = ModelLink(state.clone());
        let barrier = ModelBarrier(state.clone());
        let route_table = ModelRoutes(state.clone());
        let mut diag = ModelDiag(state.clone());

        let report = restore_with(
            &gate,
            &link,
            &barrier,
            &mut diag,
            &store,
            &route_table,
            CLEANUP_BUDGET,
        )
        .await
        .unwrap();
        assert!(!report.verified_armed);
        let state = state.lock().unwrap();
        assert_eq!(
            state.events,
            vec![
                "gate-verify",
                "gate-close",
                "link-down",
                "rx-barrier",
                "cookie-destroy",
                "gate-open",
                "link-up",
                "routes-reinstate"
            ]
        );
        assert!(!state.gate_closed);
        assert!(!state.link_down);
    }

    #[tokio::test]
    async fn failed_cookie_destroy_keeps_the_restored_clone_closed() {
        let stale = socket(23, [198, 51, 100, 8]);
        let state = Arc::new(Mutex::new(ModelState {
            gate_closed: true,
            link_down: true,
            live: vec![stale],
            fail_destroy: true,
            ..Default::default()
        }));
        let store = MemoryStore::default();
        store
            .save(&SnapshotNetworkManifest {
                version: MANIFEST_VERSION,
                routes: model_routes(),
                sockets: vec![stale],
            })
            .unwrap();
        let gate = ModelGate(state.clone());
        let link = ModelLink(state.clone());
        let barrier = ModelBarrier(state.clone());
        let route_table = ModelRoutes(state.clone());
        state.lock().unwrap().live_routes = model_routes();
        let mut diag = ModelDiag(state.clone());

        restore_with(
            &gate,
            &link,
            &barrier,
            &mut diag,
            &store,
            &route_table,
            CLEANUP_BUDGET,
        )
        .await
        .expect_err("a failed exact destroy must fail restore closed");
        let state = state.lock().unwrap();
        assert_eq!(
            state.events,
            vec!["gate-verify", "link-down", "rx-barrier", "cookie-destroy"]
        );
        assert!(state.gate_closed, "failed restore published ingress");
        assert!(state.link_down, "failed restore raised the external link");
        assert!(
            store.load().is_ok(),
            "failed restore removed its identity manifest"
        );
    }

    #[tokio::test]
    async fn failed_manifest_retirement_keeps_the_restored_clone_closed() {
        let stale = socket(24, [198, 51, 100, 8]);
        let state = Arc::new(Mutex::new(ModelState {
            gate_closed: true,
            link_down: true,
            live: vec![stale],
            ..Default::default()
        }));
        let store = RejectingRemoveStore(MemoryStore::default());
        store
            .save(&SnapshotNetworkManifest {
                version: MANIFEST_VERSION,
                routes: model_routes(),
                sockets: vec![stale],
            })
            .unwrap();
        let gate = ModelGate(state.clone());
        let link = ModelLink(state.clone());
        let barrier = ModelBarrier(state.clone());
        let route_table = ModelRoutes(state.clone());
        state.lock().unwrap().live_routes = model_routes();
        let mut diag = ModelDiag(state.clone());

        let error = restore_with(
            &gate,
            &link,
            &barrier,
            &mut diag,
            &store,
            &route_table,
            CLEANUP_BUDGET,
        )
        .await
        .expect_err("manifest retirement failure must prevent clone publication");
        assert!(
            format!("{error:#}").contains("retiring snapshot network manifest"),
            "unexpected diagnostic: {error:#}"
        );
        let state = state.lock().unwrap();
        assert_eq!(
            state.events,
            vec!["gate-verify", "link-down", "rx-barrier", "cookie-destroy"]
        );
        assert!(state.gate_closed);
        assert!(state.link_down);
    }

    #[tokio::test]
    async fn exhausted_cleanup_budget_keeps_the_restored_clone_closed() {
        let stale = socket(25, [198, 51, 100, 8]);
        let state = Arc::new(Mutex::new(ModelState {
            gate_closed: true,
            link_down: true,
            live: vec![stale],
            ..Default::default()
        }));
        let store = MemoryStore::default();
        store
            .save(&SnapshotNetworkManifest {
                version: MANIFEST_VERSION,
                routes: model_routes(),
                sockets: vec![stale],
            })
            .unwrap();
        let gate = ModelGate(state.clone());
        let link = ModelLink(state.clone());
        let barrier = ModelBarrier(state.clone());
        let route_table = ModelRoutes(state.clone());
        state.lock().unwrap().live_routes = model_routes();
        let mut diag = ModelDiag(state.clone());

        // Each destroy carries a five-second receive timeout, so an unbounded
        // loop over a large manifest can hold the clone indefinitely. With no
        // budget left the cleanup must fail closed before touching a socket,
        // never publish, and never retire the manifest.
        let error = restore_with(
            &gate,
            &link,
            &barrier,
            &mut diag,
            &store,
            &route_table,
            std::time::Duration::ZERO,
        )
        .await
        .expect_err("an exhausted cleanup budget must fail restore closed");
        assert!(
            format!("{error:#}").contains("exceeded its"),
            "unexpected diagnostic: {error:#}"
        );

        let state = state.lock().unwrap();
        assert_eq!(state.events, vec!["gate-verify", "link-down", "rx-barrier"]);
        assert!(state.gate_closed, "a timed-out cleanup published ingress");
        assert!(
            state.link_down,
            "a timed-out cleanup raised the external link"
        );
        assert!(
            store.load().is_ok(),
            "a timed-out cleanup retired its identity manifest"
        );
    }

    #[test]
    fn snapshot_network_transactions_are_serialized_by_one_guest_lock() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("snapshot-network.lock");
        let first = acquire_boundary_lock_at(&path).unwrap();
        let (attempting_tx, attempting_rx) = std::sync::mpsc::channel();
        let (acquired_tx, acquired_rx) = std::sync::mpsc::channel();
        let second_path = path.clone();
        let thread = std::thread::spawn(move || {
            attempting_tx.send(()).unwrap();
            let _second = acquire_boundary_lock_at(&second_path).unwrap();
            acquired_tx.send(()).unwrap();
        });

        attempting_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("second transaction did not attempt the lock");
        assert!(
            matches!(
                acquired_rx.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ),
            "second transaction crossed the first transaction's lock"
        );
        drop(first);
        acquired_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("second transaction did not acquire after unlock");
        thread.join().unwrap();
    }

    #[test]
    fn restore_cleanup_rejects_false_ss_stdout_destroy_confirmation() {
        let sequence = 77;
        let displayed = InetDiagMessage {
            family: libc::AF_INET as u8,
            state: 1,
            timer: 0,
            retransmits: 0,
            id: socket(31, [198, 51, 100, 8]).id,
            expires: 0,
            receive_queue: 0,
            write_queue: 0,
            uid: 0,
            inode: 1,
        };
        let header = NetlinkHeader {
            length: (std::mem::size_of::<NetlinkHeader>() + std::mem::size_of::<InetDiagMessage>())
                as u32,
            message_type: SOCK_DIAG_BY_FAMILY,
            flags: 0,
            sequence,
            port_id: 0,
        };
        let mut bytes = Vec::new();
        append_netlink_header(&mut bytes, header);
        append_diag_message(&mut bytes, displayed);

        let error = parse_destroy_reply(&bytes, sequence)
            .expect_err("a displayed tuple without NLMSG_ERROR ACK must fail closed");
        assert!(
            format!("{error:#}").contains("not a destruction oracle"),
            "unexpected diagnostic: {error:#}"
        );
    }

    #[test]
    fn netlink_ack_is_the_only_positive_destroy_oracle() {
        let sequence = 91;
        let request = NetlinkHeader {
            length: (std::mem::size_of::<NetlinkHeader>() + std::mem::size_of::<InetDiagRequest>())
                as u32,
            message_type: SOCK_DESTROY,
            flags: NLM_F_REQUEST | NLM_F_ACK,
            sequence,
            port_id: 0,
        };
        let header = NetlinkHeader {
            length: (std::mem::size_of::<NetlinkHeader>()
                + std::mem::size_of::<i32>()
                + std::mem::size_of::<NetlinkHeader>()) as u32,
            message_type: NLMSG_ERROR,
            flags: 0,
            sequence,
            port_id: 0,
        };
        let mut bytes = Vec::new();
        append_netlink_header(&mut bytes, header);
        bytes.extend_from_slice(&0i32.to_ne_bytes());
        append_netlink_header(&mut bytes, request);
        assert_eq!(
            parse_destroy_reply(&bytes, sequence).unwrap(),
            Some(DestroyOutcome::Destroyed)
        );
    }

    #[test]
    fn dump_completion_error_rejects_a_partial_cookie_manifest() {
        let header = NetlinkHeader {
            length: (std::mem::size_of::<NetlinkHeader>() + std::mem::size_of::<i32>()) as u32,
            message_type: NLMSG_DONE,
            flags: 0,
            sequence: 101,
            port_id: 0,
        };
        let error = validate_dump_completion(header, &(-libc::EINTR).to_ne_bytes())
            .expect_err("an errored dump completion must never publish a partial manifest");
        let diagnostic = format!("{error:#}");
        assert!(
            diagnostic.contains("errno 4"),
            "unexpected diagnostic: {error:#}"
        );
        assert!(
            diagnostic.contains("incomplete manifest"),
            "unexpected diagnostic: {error:#}"
        );
    }

    #[test]
    fn interrupted_dump_flag_rejects_a_partial_cookie_manifest() {
        let header = NetlinkHeader {
            length: std::mem::size_of::<NetlinkHeader>() as u32,
            message_type: NLMSG_DONE,
            flags: NLM_F_DUMP_INTR,
            sequence: 102,
            port_id: 0,
        };
        let error = validate_dump_completion(header, &[])
            .expect_err("an interrupted dump must never publish a partial manifest");
        assert!(
            format!("{error:#}").contains("dump was interrupted"),
            "unexpected diagnostic: {error:#}"
        );
    }
}
