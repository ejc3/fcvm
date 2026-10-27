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
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::netlink::{
    append_netlink_header, decode_netlink_error, for_each_netlink_message, netlink_align,
    read_dump, read_unaligned, validate_error_request, KernelChannel, NetlinkChannel,
    NetlinkHeader, OnInterrupt, NEXT_NETLINK_SEQUENCE, NLMSG_DONE, NLMSG_ERROR, NLMSG_NOOP,
    NLMSG_OVERRUN, NLM_F_ACK, NLM_F_DUMP, NLM_F_REQUEST,
};
use crate::nftables::{Family, GateHalf, Removal};

const MANIFEST_VERSION: u32 = 1;
const MANIFEST_PATH: &str = "/run/fcvm/snapshot-network.json";
pub(crate) const SYSFS_NET_PATH: &str = "/sys/class/net";
const BOUNDARY_LOCK_PATH: &str = "/run/fcvm/snapshot-network.lock";
const GATE_INPUT_CHAIN: &str = "FCVM_SNAPSHOT_IN";
const GATE_OUTPUT_CHAIN: &str = "FCVM_SNAPSHOT_OUT";

const NETLINK_SOCK_DIAG: i32 = 4;
const SOCK_DIAG_BY_FAMILY: u16 = 20;
const SOCK_DESTROY: u16 = 21;
const IPPROTO_TCP: u8 = 6;
const TCP_LISTEN: u8 = 10;
const TCP_STATE_MAX: u8 = 12;
const INET_DIAG_NOCOOKIE: u32 = u32::MAX;

/// Distinct from the netlink sequence: this only has to make a temporary
/// manifest filename unique within one process, and sharing the netlink counter
/// coupled two unrelated identifiers.
static NEXT_MANIFEST_TEMP_ID: AtomicU32 = AtomicU32::new(1);

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
    /// Let the guest's own new connections out again while new inbound
    /// connections stay rejected: remove the OUTPUT direction of the gate in
    /// both address families and keep the INPUT direction. A restored clone
    /// opens this half before its restore hook runs, so the hook, and the
    /// workload's reconnects behind it, reach the network, while nothing
    /// from outside reaches the clone until `open()` publishes it.
    async fn open_egress(&self) -> Result<()>;
    /// Remove whatever is left of the gate in both families: both directions,
    /// or only INPUT after `open_egress()`.
    async fn open(&self) -> Result<()>;
    /// Restore side only: also refuse client-direction UDP while ingress is
    /// gated (`udp_ingress_rule_args`). The snapshot side never calls this: a
    /// snapshot must not disturb the VM it is taken from, and this rule would
    /// answer a live client's datagram with an ICMP unreachable.
    async fn gate_udp_ingress(&self) -> Result<()>;
    /// Report whether the gate is already fully installed: both directional
    /// chains present with exactly the boundary rules, each hooked from its
    /// built-in chain, in both address families.
    async fn is_closed(&self) -> Result<bool>;
    /// What each gate operation has cost so far in this transaction.
    fn timings(&self) -> GateTimings {
        GateTimings::default()
    }
}

trait PacketReceiveBarrier {
    fn synchronize(&self) -> Result<()>;
}

trait CommandRunner {
    async fn output(&self, program: &str, args: &[&str]) -> io::Result<std::process::Output>;

    /// Run with `input` on stdin. For `iptables-restore`, which commits what
    /// it reads as one transaction.
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
        // A child that exits before reading all of its input breaks the pipe.
        // Its exit status and stderr say why, so a failed child is reported
        // through them; the write error stands only when the child succeeded.
        let written = stdin.write_all(input).await;
        drop(stdin);
        let output = child.wait_with_output().await?;
        match written {
            Err(error) if output.status.success() => Err(error),
            _ => Ok(output),
        }
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

struct IptablesGate<R, N: NetfilterOpener = SystemNetfilter> {
    runner: R,
    /// Opens the nfnetlink socket the gate's removal is committed on.
    netfilter: N,
    /// Per family and direction, what this gate knows of that half from
    /// iptables: installed (a verification that found the gate closed, or a
    /// close that succeeded), removed by this gate, or not known. A removal
    /// that finds a half missing from nf_tables refuses when iptables reports
    /// it installed, and asks iptables when it is not known: iptables and
    /// nf_tables disagreeing means the gate lives somewhere this removal does
    /// not reach (an iptables-legacy binary, for one), and an unpublished
    /// clone is the failure a restore can report. Indexed
    /// `[family][direction]`. Guards the array only, never held across an
    /// await.
    halves: std::sync::Mutex<[[HalfKnown; 2]; 2]>,
    /// What each gate operation of this transaction cost.
    timings: std::sync::Mutex<GateTimings>,
}

/// Lends out the netlink socket nf_tables batches are sent on.
///
/// The socket is kept, not released, between uses. Releasing an nfnetlink
/// socket runs nf_tables' NETLINK_URELEASE notifier (`nft_rcv_nl_event`),
/// which takes the namespace's commit mutex and, holding it, flushes the
/// pending transaction destruction, and the destruction a rule-deleting commit
/// schedules begins with `synchronize_rcu()`. So a release after a removal
/// waits one RCU grace period, and every nf_tables commit in the namespace
/// waits behind it. That was the whole cost of opening the gate (#1151): each
/// `iptables-restore` did its work in about 3 ms and then waited 26, 46 or
/// 79 ms (2, 4, 8 vCPUs) in close() at exit, twice per half. A release
/// between the halves put the ingress commit behind that mutex (20 ms median
/// at 2 vCPUs); a release on another thread raced with process spawns, whose
/// exec then made the final release and waited (see `network::packet_sender`).
/// A socket that is never released costs none of it.
trait NetfilterOpener {
    type Channel: NetlinkChannel + Send + 'static;
    /// The kept socket, or a new one.
    fn take(&self) -> Result<Self::Channel>;
    /// Keep `channel` for the next `take`. A socket that failed a request is
    /// dropped instead of given back: its queue may hold replies nobody read.
    fn give_back(&self, channel: Self::Channel);
}

struct SystemNetfilter;

/// fc-agent's one nfnetlink socket, between uses. The guest-wide boundary
/// lock serializes the gate transactions that use it.
static NETFILTER_SOCKET: std::sync::Mutex<Option<KernelChannel>> = std::sync::Mutex::new(None);

impl NetfilterOpener for SystemNetfilter {
    type Channel = KernelChannel;

    fn take(&self) -> Result<KernelChannel> {
        let kept = NETFILTER_SOCKET.lock().unwrap().take();
        match kept {
            Some(channel) => Ok(channel),
            None => KernelChannel::open(crate::nftables::NETLINK_NETFILTER, "nf_tables netlink"),
        }
    }

    fn give_back(&self, channel: KernelChannel) {
        *NETFILTER_SOCKET.lock().unwrap() = Some(channel);
    }
}

/// Milliseconds each gate operation of one restore took, carried into the
/// restore ACK's phase record so a regression in any one of them is visible
/// per restore. Verification and the UDP rule are process spawns per family;
/// each removal is a read of the rule handles and one commit for both
/// families.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct GateTimings {
    pub verify_ipv4_ms: f64,
    pub verify_ipv6_ms: f64,
    pub udp_ipv4_ms: f64,
    pub udp_ipv6_ms: f64,
    pub egress_read_ms: f64,
    pub egress_commit_ms: f64,
    pub ingress_read_ms: f64,
    pub ingress_commit_ms: f64,
}

impl<R> IptablesGate<R, SystemNetfilter> {
    fn new(runner: R) -> Self {
        Self::with_netfilter(runner, SystemNetfilter)
    }
}

impl<R, N: NetfilterOpener> IptablesGate<R, N> {
    fn with_netfilter(runner: R, netfilter: N) -> Self {
        Self {
            runner,
            netfilter,
            halves: std::sync::Mutex::new([[HalfKnown::Unknown; 2]; 2]),
            timings: std::sync::Mutex::new(GateTimings::default()),
        }
    }

    fn mark(&self, family: Family, directions: &[Direction], known: HalfKnown) {
        let mut halves = self.halves.lock().unwrap();
        for direction in directions {
            halves[family_index(family)][direction_index(*direction)] = known;
        }
    }

    fn known(&self, family: Family, direction: Direction) -> HalfKnown {
        self.halves.lock().unwrap()[family_index(family)][direction_index(direction)]
    }
}

/// What a gate knows of one half of itself in one family (see
/// `IptablesGate::halves`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HalfKnown {
    Unknown,
    Installed,
    Removed,
}

fn family_index(family: Family) -> usize {
    match family {
        Family::Ipv4 => 0,
        Family::Ipv6 => 1,
    }
}

fn direction_index(direction: Direction) -> usize {
    match direction {
        Direction::Input => 0,
        Direction::Output => 1,
    }
}

/// Why a gate iptables sees and nf_tables does not hold fails the removal.
const ELSEWHERE: &str = "the gate is not where this removal can reach it (is the guest's \
     iptables the nf_tables backend?), so the clone stays unpublished";

fn program_of(family: Family) -> &'static str {
    match family {
        Family::Ipv4 => "iptables",
        Family::Ipv6 => "ip6tables",
    }
}

fn family_of(program: &str) -> Family {
    if program.starts_with("ip6") {
        Family::Ipv6
    } else {
        Family::Ipv4
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
///
/// Loopback passes in both chains: the direction's own end (`loopback_flag`:
/// `-s` for INPUT, `-d` for OUTPUT) is the guest talking to itself. INPUT also
/// passes whatever enters on `lo`: the transparent egress proxy's redirected
/// flows, which the nat table sent to the proxy's loopback listener with the
/// guest's own address as their source, and which INPUT would otherwise reset
/// while ingress is gated. The exemption is by interface, not by destination:
/// a published-port SYN is DNATed to a loopback destination in PREROUTING and
/// arrives on eth0, and must still be rejected while the gate is closed.
///
/// These are the rules both the snapshot side and the restore side carry, and
/// what verification expects. UDP ingress is not among them: a restored clone
/// adds its own UDP rejection to INPUT with `gate_udp_ingress` (see there), which
/// the snapshot side must not install, because it would answer a live client's
/// datagram on the source VM with an ICMP unreachable during the cookie dump.
///
/// OUTPUT also rejects the guest's own NTP requests (UDP to port 123). chronyd
/// steps the clock at any update (`makestep 1 -1`), and a restored clone is
/// behind by the snapshot's age, so a poll answered while egress is gated would
/// jump the clock while every daemon whose session ran out in the jump is
/// refused a connection: the window the restore keeps its own clock step out
/// of. The rule has to be in the snapshot, not added by the restore, so that no
/// poll can leave between the resume and the restore's first step while the
/// inherited gate verifies. (A reinstall over a gate that fails verification
/// replaces each chain in one transaction, `configure_gate_chains`, so the rule
/// holds through it too.) A REJECT
/// in OUTPUT fails the send itself (`sendto` returns EPERM), so on the source
/// VM a poll whose timer fires during the cookie dump fails and chronyd logs
/// "Could not send". The source asks chronyd for a burst once egress is open
/// again (`chrony_burst`), so it does not wait a poll interval; a clone starts
/// a fresh chronyd after its clock step, which polls at once.
fn gate_chain_rules(loopback_flag: &str, loopback_network: &str) -> Vec<Vec<String>> {
    let owned = |tokens: &[&str]| tokens.iter().map(|token| token.to_string()).collect();
    let mut rules = vec![owned(&[
        "-p",
        "tcp",
        loopback_flag,
        loopback_network,
        "-j",
        "RETURN",
    ])];
    let ingress = loopback_flag == "-s";
    if ingress {
        rules.push(owned(&["-i", "lo", "-j", "RETURN"]));
    } else {
        // `-m udp` is what `iptables -S` renders for `--dport`, and spelling it
        // here keeps the rule equal to its rendering for verification.
        let unreachable = port_unreachable(loopback_network);
        rules.push(owned(&[
            "-p",
            "udp",
            "-m",
            "udp",
            "--dport",
            "123",
            "-j",
            "REJECT",
            "--reject-with",
            unreachable,
        ]));
    }
    rules.extend([owned(&[
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
    ])]);
    rules.push(owned(&["-j", "RETURN"]));
    rules
}

/// Ask chronyd to measure now: four polls, four answers needed. Run once the
/// gate's egress half is open, because a poll the gate rejected leaves chronyd
/// waiting its whole poll interval (up to 1024 s) to measure again.
async fn chrony_burst() {
    let _ = tokio::process::Command::new("chronyc")
        .args(["burst", "4/4"])
        .output()
        .await;
}

/// The ICMP reply a UDP REJECT sends, by the family of `loopback_network`.
fn port_unreachable(loopback_network: &str) -> &'static str {
    if loopback_network.contains(':') {
        "icmp6-port-unreachable"
    } else {
        "icmp-port-unreachable"
    }
}

/// The restored clone's UDP rejection, inserted into the INPUT gate chain below
/// the two loopback exemptions: a published UDP port reaches the workload
/// through the port forwarders like a TCP one, and a client's datagram while
/// ingress is gated would read or change state the restore hook is about to
/// repair. The match is the flow's direction, not the conntrack state, because
/// a client that exchanged traffic before the snapshot inherits an ESTABLISHED
/// entry in the restored guest and a NEW-only rule would pass it. Replies to
/// the guest's own outbound UDP are reply-direction and pass; OUTPUT gates no
/// UDP but NTP (`gate_chain_rules`), so DNS works throughout. Removed with the
/// chain when ingress opens.
fn udp_ingress_rule_args(loopback_network: &str) -> [&'static str; 10] {
    let unreachable = port_unreachable(loopback_network);
    [
        "-p",
        "udp",
        "-m",
        "conntrack",
        "--ctdir",
        "ORIGINAL",
        "-j",
        "REJECT",
        "--reject-with",
        unreachable,
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

impl<R: CommandRunner, N: NetfilterOpener> IptablesGate<R, N> {
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
        let started = std::time::Instant::now();
        let state = self.read_family_state(program).await?;
        let closed = family_gate_is_closed(&state, loopback_network);
        let family = family_of(program);
        let known = if closed {
            HalfKnown::Installed
        } else {
            HalfKnown::Unknown
        };
        self.mark(family, &[Direction::Input, Direction::Output], known);
        let elapsed = started.elapsed().as_secs_f64() * 1000.0;
        let mut timings = self.timings.lock().unwrap();
        match family {
            Family::Ipv4 => timings.verify_ipv4_ms = elapsed,
            Family::Ipv6 => timings.verify_ipv6_ms = elapsed,
        }
        Ok(closed)
    }

    /// Replace both gate chains of one family with exactly the gate's rules,
    /// in one `iptables-restore --noflush` transaction. Declaring a chain
    /// creates it, or flushes that chain only when it exists, and the table
    /// is committed whole on both the legacy and nf_tables backends. So a
    /// chain that is already hooked from INPUT or OUTPUT, which is what a
    /// restored clone reinstalls over when its inherited gate fails
    /// verification, goes from its old rules to the gate's with no moment in
    /// between. Flushing it and appending one rule per process left it empty,
    /// and so admitting every new flow and NTP poll, until the last append
    /// (issue 1244).
    async fn configure_gate_chains(&self, program: &str, loopback_network: &str) -> Result<()> {
        let mut input = String::from("*filter\n");
        for (chain, loopback_flag) in [(GATE_INPUT_CHAIN, "-s"), (GATE_OUTPUT_CHAIN, "-d")] {
            input.push_str(&format!(":{chain} - [0:0]\n"));
            for rule in gate_chain_rules(loopback_flag, loopback_network) {
                input.push_str(&format!("-A {chain} {}\n", rule.join(" ")));
            }
        }
        input.push_str("COMMIT\n");
        let restore = format!("{program}-restore");
        let args = ["-w", "--noflush"];
        let output = self
            .runner
            .output_with_input(&restore, &args, input.as_bytes())
            .await
            .with_context(|| format!("spawning {restore}"))?;
        if !output.status.success() {
            bail!(
                "snapshot network gate command failed: {}",
                command_failure(&restore, &args, &output)
            );
        }
        Ok(())
    }

    async fn install_hook(&self, program: &str, hook: &str, chain: &str) -> Result<()> {
        // Always insert at position 1. `-C` can only prove the jump exists
        // SOMEWHERE, and a jump below a foreign rule is a gate that foreign
        // rule can bypass, so close() must put a copy ahead of everything.
        // A duplicate left lower in the chain is harmless (the first match
        // governs) and the removal retires every copy it finds.
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
        self.configure_gate_chains(program, loopback_network)
            .await?;
        self.install_hook(program, "INPUT", GATE_INPUT_CHAIN)
            .await?;
        self.install_hook(program, "OUTPUT", GATE_OUTPUT_CHAIN)
            .await?;
        self.mark(
            family_of(program),
            &[Direction::Input, Direction::Output],
            HalfKnown::Installed,
        );
        Ok(())
    }

    /// Remove the named halves of the gate in both families as ONE nf_tables
    /// transaction: read the hook rules' handles and the chains of each half
    /// in each family, then commit one batch that deletes the hooks, flushes
    /// and deletes the chains, in both families at once. Either everything
    /// read is removed or, if anything changed since the read, the batch
    /// fails whole and nothing is; the restore then fails closed.
    ///
    /// This used to be one `iptables-restore --noflush` per family: two
    /// commits per half, with one family open and the other not between them,
    /// and each process waited one RCU grace period on exit. The socket here
    /// is kept and never released (`NetfilterOpener`).
    ///
    /// `directions` names which halves to remove. The egress half alone
    /// (`[Direction::Output]`) is what a restored clone removes before its
    /// hook runs; the ingress half follows at publication.
    async fn remove_halves(&self, directions: &[Direction]) -> Result<()> {
        // A batch is pinned to the generation its reads saw. A commit anywhere
        // in the namespace in between (a workload's own firewall change)
        // refuses it whole, and it is read and built again. Bounded so a
        // ruleset that never holds still fails closed instead of spinning.
        const ATTEMPTS: usize = 8;
        let read_started = std::time::Instant::now();
        let mut channel = self.netfilter.take()?;
        let half_name = if directions.contains(&Direction::Input) {
            "ingress"
        } else {
            "egress"
        };
        let mut attempt = 0;
        let (read_ms, commit_started) = loop {
            attempt += 1;
            let generation = crate::nftables::generation(&mut channel)?;
            let mut removals = Vec::new();
            for family in Family::BOTH {
                for direction in directions {
                    let half = direction.half();
                    let layout = crate::nftables::read_half(&mut channel, family, half)?;
                    let held = layout.chain || !layout.hooks.is_empty();
                    match self.known(family, *direction) {
                        HalfKnown::Installed if !layout.chain || layout.hooks.is_empty() => {
                            bail!(
                                "{} reported the gate's {} chain installed, but the {} \
                                 filter table in nf_tables holds {}; {}",
                                program_of(family),
                                half.chain,
                                family.name(),
                                if layout.chain {
                                    "no jump to it"
                                } else {
                                    "no such chain"
                                },
                                ELSEWHERE
                            );
                        }
                        HalfKnown::Unknown if !held => {
                            let state = self.read_family_state(program_of(family)).await?;
                            let (rules, hooks) = match direction {
                                Direction::Input => (&state.input_rules, state.input_hooks),
                                Direction::Output => (&state.output_rules, state.output_hooks),
                            };
                            if rules.is_some() || hooks > 0 {
                                bail!(
                                    "{} shows the gate's {} chain, but the {} filter table \
                                     in nf_tables does not hold it; {}",
                                    program_of(family),
                                    half.chain,
                                    family.name(),
                                    ELSEWHERE
                                );
                            }
                        }
                        _ => {}
                    }
                    removals.push(Removal {
                        family,
                        half,
                        layout,
                    });
                }
            }
            let read_ms = read_started.elapsed().as_secs_f64() * 1000.0;
            let commit_started = std::time::Instant::now();
            let Some((batch, _begin, end)) = crate::nftables::removal_batch(&removals, generation)
            else {
                break (read_ms, commit_started);
            };
            let outcome = crate::nftables::commit(&mut channel, &batch, end).with_context(|| {
                format!("removing the {half_name} half of the snapshot network gate in both families")
            })?;
            match outcome {
                crate::nftables::Commit::Committed => break (read_ms, commit_started),
                crate::nftables::Commit::GenerationMoved if attempt < ATTEMPTS => continue,
                crate::nftables::Commit::GenerationMoved => bail!(
                    "removing the {half_name} half of the snapshot network gate: the ruleset \
                     changed under each of {ATTEMPTS} attempts"
                ),
            }
        };
        let commit_ms = commit_started.elapsed().as_secs_f64() * 1000.0;
        // Kept for the next removal, never released (see `NetfilterOpener`).
        // Every error above returned with the socket dropped.
        self.netfilter.give_back(channel);
        for family in Family::BOTH {
            self.mark(family, directions, HalfKnown::Removed);
        }
        let mut timings = self.timings.lock().unwrap();
        if directions.contains(&Direction::Input) {
            timings.ingress_read_ms = read_ms;
            timings.ingress_commit_ms = commit_ms;
        } else {
            timings.egress_read_ms = read_ms;
            timings.egress_commit_ms = commit_ms;
        }
        Ok(())
    }
}

/// One half of the NEW-flow gate: the INPUT chain's half rejects new inbound
/// connections, the OUTPUT chain's half new outbound ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Input,
    Output,
}

impl Direction {
    fn half(self) -> GateHalf {
        match self {
            Direction::Input => GateHalf {
                hook: "INPUT",
                chain: GATE_INPUT_CHAIN,
            },
            Direction::Output => GateHalf {
                hook: "OUTPUT",
                chain: GATE_OUTPUT_CHAIN,
            },
        }
    }
}

impl<R: CommandRunner + Sync, N: NetfilterOpener + Sync> FirewallGate for IptablesGate<R, N> {
    async fn close(&self) -> Result<()> {
        self.install_family("iptables", IPV4_LOOPBACK)
            .await
            .context("installing IPv4 snapshot network gate")?;
        self.install_family("ip6tables", IPV6_LOOPBACK)
            .await
            .context("installing IPv6 snapshot network gate")
    }

    async fn open_egress(&self) -> Result<()> {
        self.remove_halves(&[Direction::Output]).await
    }

    async fn open(&self) -> Result<()> {
        self.remove_halves(&[Direction::Input, Direction::Output])
            .await
    }

    async fn gate_udp_ingress(&self) -> Result<()> {
        for (program, network) in [("iptables", IPV4_LOOPBACK), ("ip6tables", IPV6_LOOPBACK)] {
            // Position 3: below the loopback exemptions (rules 1 and 2), above
            // the TCP rejection. The chain is removed whole on open, so the
            // rule needs no removal of its own.
            let started = std::time::Instant::now();
            let mut args = vec!["-w", "-I", GATE_INPUT_CHAIN, "3"];
            args.extend(udp_ingress_rule_args(network));
            self.required(program, &args)
                .await
                .with_context(|| format!("gating client-direction UDP in {program}"))?;
            let elapsed = started.elapsed().as_secs_f64() * 1000.0;
            let mut timings = self.timings.lock().unwrap();
            match family_of(program) {
                Family::Ipv4 => timings.udp_ipv4_ms = elapsed,
                Family::Ipv6 => timings.udp_ipv6_ms = elapsed,
            }
        }
        Ok(())
    }

    async fn is_closed(&self) -> Result<bool> {
        // Reads only: safe to overlap regardless of backend.
        let (ipv4, ipv6) = tokio::join!(
            self.family_is_closed("iptables", IPV4_LOOPBACK),
            self.family_is_closed("ip6tables", IPV6_LOOPBACK)
        );
        Ok(ipv4? && ipv6?)
    }

    fn timings(&self) -> GateTimings {
        *self.timings.lock().unwrap()
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
/// `datagrams` is how many SOCK_DESTROY datagrams carried the requests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CleanupTally {
    pub(crate) destroyed: usize,
    pub(crate) already_gone: usize,
    pub(crate) datagrams: usize,
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
    /// Milliseconds removing the gate's egress half, after the receive barrier
    /// and before the first socket is retired, so the workload's reconnects
    /// and the hook reach out.
    pub egress_open_ms: f64,
    /// The restore hook's run: egress open, ingress still gated.
    pub hook_ms: f64,
    /// Milliseconds removing the gate's ingress half, which publishes the
    /// clone.
    pub reopen_ms: f64,
    /// What the cleanup did to the manifest's sockets.
    pub(crate) tally: CleanupTally,
    /// Per-operation gate timings, refreshed at the end of each phase.
    pub gate: GateTimings,
}

/// Total budget for retiring every socket named by the manifest.
///
/// Every wait for an acknowledgement carries a five-second receive timeout, so
/// a large manifest could otherwise hold an unpublished clone for minutes with
/// no upper bound. The budget is checked before each batch. Exceeding it fails
/// closed and names the progress made, which is diagnosable; an open-ended
/// stall is not.
const CLEANUP_BUDGET: std::time::Duration = std::time::Duration::from_secs(30);

/// Most `SOCK_DESTROY` requests sent in one datagram.
///
/// The kernel handles every request in a datagram inside that one send(2) and
/// queues one acknowledgement per request before send returns. An
/// acknowledgement that does not fit the socket's receive buffer is dropped,
/// and the next receive fails with ENOBUFS, so a batch has to fit the buffer.
/// The destroyer reserves room for a full batch before its first one, and
/// sends smaller batches when the kernel grants less.
const DESTROY_BATCH: usize = 64;

/// A bound on what one acknowledgement charges to a receive buffer. Measured
/// in the 6.18 guest kernel: 256 error acknowledgements (the larger kind, 92
/// bytes each) fill the default 212,992-byte buffer, 832 bytes each.
const ACK_CHARGE_BOUND: usize = 2048;

trait SocketDiagnostic {
    fn dump_tcp_sockets(&mut self) -> Result<Vec<TcpSocketIdentity>>;
    /// Retire `sockets` by cookie and return one outcome per socket, in order.
    /// A socket the kernel could not retire fails the whole call.
    fn destroy(&mut self, sockets: &[TcpSocketIdentity]) -> Result<Vec<DestroyOutcome>>;
    /// How many datagrams of SOCK_DESTROY requests `destroy` has sent.
    fn destroy_datagrams(&self) -> usize;
}

struct SystemSocketDiagnostic {
    destroyer: CookieDestroyer<KernelChannel, fn() -> Result<KernelChannel>>,
}

impl SystemSocketDiagnostic {
    fn new() -> Self {
        Self {
            destroyer: CookieDestroyer::new(open_diag_channel as fn() -> Result<KernelChannel>),
        }
    }
}

impl SocketDiagnostic for SystemSocketDiagnostic {
    fn dump_tcp_sockets(&mut self) -> Result<Vec<TcpSocketIdentity>> {
        let mut sockets = Vec::new();
        for family in [libc::AF_INET as u8, libc::AF_INET6 as u8] {
            sockets.extend(dump_family(family)?);
        }
        Ok(sockets)
    }

    fn destroy(&mut self, sockets: &[TcpSocketIdentity]) -> Result<Vec<DestroyOutcome>> {
        self.destroyer.destroy(sockets)
    }

    fn destroy_datagrams(&self) -> usize {
        self.destroyer.datagrams
    }
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

fn open_diag_channel() -> Result<KernelChannel> {
    KernelChannel::open(NETLINK_SOCK_DIAG, "NETLINK_SOCK_DIAG")
        .context("opening a sock_diag socket (guest kernel needs CONFIG_INET_DIAG=y)")
}

/// Bytes of one netlink message carrying an `InetDiagRequest`. Already
/// aligned, so the messages of a batch pack back to back.
const DIAG_REQUEST_MESSAGE_LEN: usize =
    std::mem::size_of::<NetlinkHeader>() + std::mem::size_of::<InetDiagRequest>();
const _: () = assert!(netlink_align(DIAG_REQUEST_MESSAGE_LEN) == DIAG_REQUEST_MESSAGE_LEN);

fn append_diag_request_message(
    bytes: &mut Vec<u8>,
    message_type: u16,
    flags: u16,
    request: &InetDiagRequest,
    sequence: u32,
) {
    let start = bytes.len();
    append_netlink_header(
        bytes,
        NetlinkHeader {
            length: DIAG_REQUEST_MESSAGE_LEN as u32,
            message_type,
            flags,
            sequence,
            port_id: 0,
        },
    );
    append_diag_request(bytes, *request);
    debug_assert_eq!(bytes.len() - start, DIAG_REQUEST_MESSAGE_LEN);
}

fn dump_family(family: u8) -> Result<Vec<TcpSocketIdentity>> {
    let mut channel = open_diag_channel()?;
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
    let mut message = Vec::with_capacity(DIAG_REQUEST_MESSAGE_LEN);
    append_diag_request_message(
        &mut message,
        SOCK_DIAG_BY_FAMILY,
        NLM_F_REQUEST | NLM_F_DUMP,
        &request,
        sequence,
    );
    channel.send(&message)?;
    let mut sockets = Vec::new();
    read_dump(
        &mut channel,
        sequence,
        SOCK_DIAG_BY_FAMILY,
        SOCK_DIAG_BY_FAMILY,
        OnInterrupt::Refuse,
        |_, payload| {
            let message: InetDiagMessage = read_unaligned(payload)?;
            if message.id.cookie == [INET_DIAG_NOCOOKIE; 2] {
                bail!("kernel returned a TCP socket without an identity cookie");
            }
            sockets.push(TcpSocketIdentity {
                family: message.family,
                state: message.state,
                id: message.id,
            });
            Ok(())
        },
        |_, _| Ok(()),
    )
    .context(
        "SOCK_DIAG dump (guest kernel needs CONFIG_INET_DIAG=y); refusing an incomplete manifest",
    )?;
    Ok(sockets)
}

/// Retires cookies over one sock_diag socket for a whole restore: the socket
/// opens with the first batch and every later batch reuses it.
struct CookieDestroyer<C, F> {
    open: F,
    channel: Option<C>,
    /// Requests per datagram: `DESTROY_BATCH`, or fewer when the socket's
    /// receive buffer cannot hold that many acknowledgements.
    batch: usize,
    /// Datagrams of SOCK_DESTROY requests sent so far.
    datagrams: usize,
}

impl<C: NetlinkChannel, F: FnMut() -> Result<C>> CookieDestroyer<C, F> {
    fn new(open: F) -> Self {
        Self {
            open,
            channel: None,
            batch: DESTROY_BATCH,
            datagrams: 0,
        }
    }

    fn destroy(&mut self, sockets: &[TcpSocketIdentity]) -> Result<Vec<DestroyOutcome>> {
        let channel = match &mut self.channel {
            Some(channel) => channel,
            empty => {
                let mut channel = (self.open)()?;
                // A guest, or a privileged workload in it, can lower
                // net.core.rmem_default below what a batch needs.
                let granted = channel
                    .reserve_receive_buffer(DESTROY_BATCH * ACK_CHARGE_BOUND)
                    .context("making room for SOCK_DESTROY acknowledgements")?;
                self.batch = (granted / ACK_CHARGE_BOUND).clamp(1, DESTROY_BATCH);
                if self.batch < DESTROY_BATCH {
                    eprintln!(
                        "[fc-agent] WARNING: the SOCK_DIAG receive buffer is {granted} bytes, \
                         so snapshot sockets are destroyed {} at a time",
                        self.batch
                    );
                }
                empty.insert(channel)
            }
        };
        let mut outcomes = Vec::with_capacity(sockets.len());
        for batch in sockets.chunks(self.batch) {
            self.datagrams += 1;
            outcomes.extend(destroy_batch(channel, batch)?);
        }
        Ok(outcomes)
    }
}

/// Destroy `sockets` by cookie: every request goes out in one datagram, and
/// each acknowledgement is matched back to its socket by sequence number.
///
/// The kernel handles all of a datagram's requests during the send and queues
/// their acknowledgements, so what remains is reading replies that are already
/// there. An outcome is recorded only from an NLMSG_ERROR acknowledgement that
/// echoes a SOCK_DESTROY request with that socket's own sequence number. The
/// first such reply decides the socket, and any other reply for an undecided
/// socket fails the batch closed. Requests after a failed one were already
/// handled by then, which changes nothing: they name snapshot-time sockets,
/// and the failure keeps the clone unpublished either way.
fn destroy_batch<C: NetlinkChannel>(
    channel: &mut C,
    sockets: &[TcpSocketIdentity],
) -> Result<Vec<DestroyOutcome>> {
    let count = u32::try_from(sockets.len()).context("SOCK_DESTROY batch is too large")?;
    let first_sequence = NEXT_NETLINK_SEQUENCE.fetch_add(count, Ordering::Relaxed);
    let mut datagram = Vec::with_capacity(sockets.len() * DIAG_REQUEST_MESSAGE_LEN);
    for (offset, socket) in (0u32..).zip(sockets) {
        let request = InetDiagRequest {
            family: socket.family,
            protocol: IPPROTO_TCP,
            extensions: 0,
            pad: 0,
            states: 0,
            id: socket.id,
        };
        append_diag_request_message(
            &mut datagram,
            SOCK_DESTROY,
            NLM_F_REQUEST | NLM_F_ACK,
            &request,
            first_sequence.wrapping_add(offset),
        );
    }
    channel.send(&datagram)?;
    let mut outcomes = vec![None; sockets.len()];
    let mut undecided = sockets.len();
    while undecided > 0 {
        let replies = channel.receive().with_context(|| {
            format!(
                "waiting for SOCK_DESTROY acknowledgements: {undecided} of {} undecided",
                sockets.len()
            )
        })?;
        for_each_netlink_message(replies, |header, payload| {
            let index = header.sequence.wrapping_sub(first_sequence) as usize;
            // Skip anything that is not a reply to an undecided request of
            // this batch.
            let Some(slot) = outcomes.get_mut(index) else {
                return Ok(false);
            };
            if slot.is_some() {
                return Ok(false);
            }
            let outcome = parse_destroy_reply(header, payload).with_context(|| {
                format!(
                    "destroying snapshot-time TCP socket {}",
                    sockets[index].describe()
                )
            })?;
            if let Some(outcome) = outcome {
                *slot = Some(outcome);
                undecided -= 1;
            }
            Ok(false)
        })?;
    }
    outcomes
        .into_iter()
        .collect::<Option<Vec<_>>>()
        .context("SOCK_DESTROY batch ended with an undecided socket")
}

/// Classify one reply to a SOCK_DESTROY: the outcome for an acknowledgement,
/// `None` for a NOOP, and an error for anything else.
fn parse_destroy_reply(header: NetlinkHeader, payload: &[u8]) -> Result<Option<DestroyOutcome>> {
    match header.message_type {
        NLMSG_ERROR => {
            let (error, request) = decode_netlink_error(payload)?;
            validate_error_request(request, header.sequence, SOCK_DESTROY)?;
            if error == 0 {
                return Ok(Some(DestroyOutcome::Destroyed));
            }
            let errno = -error;
            if errno == libc::ENOENT || errno == libc::ESTALE {
                return Ok(Some(DestroyOutcome::AlreadyGone));
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
        NLMSG_NOOP => Ok(None),
        NLMSG_OVERRUN => bail!("SOCK_DESTROY acknowledgement overran receive buffer"),
        NLMSG_DONE => bail!("SOCK_DESTROY ended without a netlink acknowledgement"),
        other => bail!(
            "SOCK_DESTROY returned message type {other} without NLMSG_ERROR acknowledgement; \
             displayed socket data is not a destruction oracle"
        ),
    }
}

/// Publish the network again by removing the gate, and nothing else.
///
/// Nothing else is left to do: with the link-down gone the boundary destroys no
/// kernel state, so there are no routes and no neighbours to put back. This
/// function used to reinstate routes, and a neighbour repair was on its way into
/// it before the purge itself was removed instead.
async fn reopen_with<G: FirewallGate>(gate: &G) -> Result<()> {
    // One nf_tables transaction for both families: a failure removes nothing,
    // the caller reports it and the clone stays unpublished.
    gate.open()
        .await
        .context("opening the snapshot NEW-flow gate")
}

async fn rollback_preparation<G: FirewallGate>(error: anyhow::Error, gate: &G) -> anyhow::Error {
    match reopen_with(gate).await {
        Ok(()) => error,
        Err(reopen) => {
            anyhow::anyhow!("{error:#}; additionally failed to restore source network: {reopen:#}")
        }
    }
}

async fn prepare_with<
    G: FirewallGate,
    B: PacketReceiveBarrier,
    D: SocketDiagnostic,
    S: ManifestStore,
>(
    gate: &G,
    receive_barrier: &B,
    diagnostic: &mut D,
    store: &S,
) -> Result<usize> {
    // THE VM BEING SNAPSHOTTED IS NOT DISTURBED. A caller asked for a copy; it
    // must not get its live VM reconfigured to produce one.
    //
    // The boundary needs exactly two things: no NEW socket may appear while the
    // cookie dump runs, and packets already in receive processing must drain
    // first. The gate delivers the first (a directional NEW-flow REJECT) and the
    // AF_PACKET barrier the second. Both are reversible and destroy no kernel
    // state: the gate is two iptables chains added then removed, the barrier an
    // open/close whose release runs synchronize_net().
    //
    // `link.down()` used to sit between them. It prevented sockets the gate
    // already rejects, and purged the device's routes AND neighbours to do it.
    // Both purges reached production as silent failures: routes in ee3f5c30
    // ("a restored clone came up reachable at L2 and routed nowhere"), and
    // neighbours on 2026-08-15, where the guest re-ARPed for the bridge,
    // sometimes learned pasta's MAC, answered inbound SYNs to the wrong MAC and
    // had them RST -- a published port dead ~1 snapshot in 10 with no drop
    // counted anywhere. Removing the purge removes the class, which is why the
    // route capture went with it instead of gaining a neighbour-shaped sibling.
    if let Err(error) = gate
        .close()
        .await
        .context("closing the snapshot NEW-flow gate")
    {
        return Err(rollback_preparation(error, gate).await);
    }
    if let Err(error) = receive_barrier
        .synchronize()
        .context("waiting for pre-boundary packet receive processing")
    {
        return Err(rollback_preparation(error, gate).await);
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
        Err(error) => return Err(rollback_preparation(error, gate).await),
    };
    let manifest = SnapshotNetworkManifest {
        version: MANIFEST_VERSION,
        sockets,
        // Empty by construction: nothing purges the route table any more, so
        // there is nothing to put back. The field stays so the manifest format
        // does not fork; reinstating an empty list is a no-op.
        routes: Vec::new(),
    };
    if let Err(error) = store.save(&manifest) {
        return Err(rollback_preparation(error, gate).await);
    }
    Ok(manifest.sockets.len())
}

/// Both phases of a restore in one call: the boundary, then the publication
/// with `hook`. Production runs them apart (`RestoredNetwork`), with the NFS
/// remount and the exec and egress-proxy waits between them; the model tests
/// run them together.
#[cfg(test)]
async fn restore_with<
    G: FirewallGate,
    B: PacketReceiveBarrier,
    D: SocketDiagnostic,
    S: ManifestStore,
    H: RestoreHook,
>(
    gate: &G,
    receive_barrier: &B,
    diagnostic: &mut D,
    store: &S,
    hook: &H,
    budget: std::time::Duration,
) -> Result<RestoreNetworkReport> {
    let report = restore_boundary_with(gate, receive_barrier, diagnostic, store, budget).await?;
    publish_with(gate, report, hook).await
}

/// The restore boundary: verify the inherited gate, drain receive processing,
/// open the gate's egress half, retire the snapshot-time sockets and the
/// manifest. The clone can reach out from the moment the barrier has drained,
/// and nothing reaches it until `publish_with` opens the ingress half.
async fn restore_boundary_with<
    G: FirewallGate,
    B: PacketReceiveBarrier,
    D: SocketDiagnostic,
    S: ManifestStore,
>(
    gate: &G,
    receive_barrier: &B,
    diagnostic: &mut D,
    store: &S,
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
            .context("ensuring the restored snapshot NEW-flow gate is closed")?;
    }
    // The clone's own addition to the inherited gate: client-direction UDP is
    // refused until publication. Never part of the snapshot-side gate.
    gate.gate_udp_ingress()
        .await
        .context("gating client-direction UDP on the restored clone")?;
    // No link-down here either. The clone inherits a CLOSED gate inside the
    // artifact, so no new flow can appear while the recorded cookies are
    // destroyed, and the barrier below drains what was already in flight.
    // Downing the link would purge this clone's routes and neighbours, and the
    // boundary no longer captures routes to put them back.
    receive_barrier
        .synchronize()
        .context("waiting for restored packet receive processing to quiesce")?;
    let reassert_ms = if verified_armed {
        0.0
    } else {
        reassert_started.elapsed().as_secs_f64() * 1000.0
    };
    // Egress opens here, before the first socket is retired. Every daemon whose
    // connection the retirement kills reconnects at once; with this half still
    // closed those connects were answered with a reset (the gate rejects with
    // tcp-reset), which clients treat as a definitive refusal: ServiceRouter
    // marked the hosts down, the configerator clients fell back to system
    // configs, and a www clone served 500s for as long as it lived. A socket a
    // daemon opens from here on is not in the manifest, so the cookie-bound
    // retirement below never touches it, whatever four-tuple it reuses.
    // Ingress stays closed until publication, so no published-port client
    // reaches the clone yet.
    let egress_started = std::time::Instant::now();
    gate.open_egress()
        .await
        .context("opening the egress half of the snapshot NEW-flow gate")?;
    let egress_open_ms = egress_started.elapsed().as_secs_f64() * 1000.0;
    let destroy_started = std::time::Instant::now();
    let manifest = store.load()?;
    let started = std::time::Instant::now();
    let mut tally = CleanupTally::default();
    for batch in manifest.sockets.chunks(DESTROY_BATCH) {
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
        let retired = tally.destroyed + tally.already_gone;
        let outcomes = diagnostic.destroy(batch).with_context(|| {
            format!(
                "retiring snapshot-time TCP sockets {} to {} of {}",
                retired + 1,
                retired + batch.len(),
                manifest.sockets.len()
            )
        })?;
        if outcomes.len() != batch.len() {
            bail!(
                "cookie-bound cleanup got {} outcomes for {} sockets; the clone stays \
                 unpublished",
                outcomes.len(),
                batch.len()
            );
        }
        for outcome in outcomes {
            match outcome {
                DestroyOutcome::Destroyed => tally.destroyed += 1,
                DestroyOutcome::AlreadyGone => tally.already_gone += 1,
            }
        }
    }
    tally.datagrams = diagnostic.destroy_datagrams();
    // Retire the armed-boundary marker before publication. Leaving it behind
    // would keep the restore watcher on the restore-only control generation;
    // a removal failure therefore remains closed instead of being a warning.
    store
        .remove()
        .context("retiring snapshot network manifest after cookie-bound cleanup")?;
    let destroy_ms = destroy_started.elapsed().as_secs_f64() * 1000.0;
    Ok(RestoreNetworkReport {
        tally,
        verified_armed,
        verify_ms,
        reassert_ms,
        destroy_ms,
        egress_open_ms,
        hook_ms: 0.0,
        reopen_ms: 0.0,
        gate: gate.timings(),
    })
}

/// The publication: run the restore hook with egress open and ingress still
/// gated, then open the ingress half. Whatever the workload must redo after a
/// restore happens before a published-port client can reach it, and the hook
/// has the network to do it with. A failure keeps the clone unpublished, like
/// a failed retirement.
async fn publish_with<G: FirewallGate, H: RestoreHook>(
    gate: &G,
    mut report: RestoreNetworkReport,
    hook: &H,
) -> Result<RestoreNetworkReport> {
    let hook_started = std::time::Instant::now();
    hook.run()
        .await
        .context("running the restore hook; the clone stays unpublished")?;
    report.hook_ms = hook_started.elapsed().as_secs_f64() * 1000.0;
    let reopen_started = std::time::Instant::now();
    reopen_with(gate)
        .await
        .context("publishing restored network after cookie-bound cleanup")?;
    report.reopen_ms = reopen_started.elapsed().as_secs_f64() * 1000.0;
    report.gate = gate.timings();
    Ok(report)
}

pub async fn prepare_snapshot_network() -> Result<()> {
    let _transaction_lock = acquire_boundary_lock()?;
    let gate = IptablesGate::new(SystemCommandRunner);
    let receive_barrier = SystemPacketReceiveBarrier;
    let mut diagnostic = SystemSocketDiagnostic::new();
    let captured = prepare_with(
        &gate,
        &receive_barrier,
        &mut diagnostic,
        &FileManifestStore::default(),
    )
    .await?;
    eprintln!(
        "[fc-agent] snapshot network boundary prepared: gate=closed link=UNTOUCHED \
         external_socket_cookies={captured}"
    );
    Ok(())
}

pub async fn resume_source_network() -> Result<()> {
    let _transaction_lock = acquire_boundary_lock()?;
    let gate = IptablesGate::new(SystemCommandRunner);
    let store = FileManifestStore::default();
    // No routes to read back: the boundary never takes the link down, so it
    // purges nothing. The manifest is retired purely to clear the armed marker.
    //
    // ORDER: publish FIRST, retire the marker second. The gate is live traffic
    // policy on a VM that was only supposed to be READ; the manifest is a local
    // file. Retiring first meant a removal failure returned early with the gate
    // still closed, leaving the source unpublished until an operator noticed --
    // the same "snapshot disturbed the source" failure this boundary exists to
    // avoid, arriving by the error path instead.
    let reopened = reopen_with(&gate)
        .await
        .context("reopening source VM network");
    let retired = store
        .remove()
        .context("retiring source snapshot network manifest after publication");
    match (reopened, retired) {
        (Ok(()), Ok(())) => {}
        // Publication failed: report it, and say if the marker also survived.
        (Err(reopen), retire) => {
            return Err(match retire {
                Ok(()) => reopen,
                Err(retire) => anyhow::anyhow!("{reopen:#}; additionally {retire:#}"),
            })
        }
        // Published, but the marker survives. Still an error -- a stale marker
        // keeps the restore watcher on the restore-only control generation --
        // and the source is at least back in service.
        (Ok(()), Err(retire)) => return Err(retire),
    }
    eprintln!(
        "[fc-agent] source VM snapshot network reopened: gate=open \
         (link never taken down)"
    );
    // A poll the gate rejected would leave chronyd waiting its whole interval.
    tokio::spawn(chrony_burst());
    // The snapshot reset the vsock transport under the running agent's output writer,
    // which reconnects only when asked; nothing else asks a resumed source.
    if let Err(error) = crate::output::request_reconnect() {
        eprintln!(
            "[fc-agent] WARNING: {error:#}; the container's output after this snapshot \
             will not reach the host"
        );
    }
    Ok(())
}

/// What a restored clone runs at publication: after the snapshot-time sockets
/// are retired and the gate's egress half is open, and before the ingress half
/// opens. A process that must redo work the snapshot made stale does it here,
/// with the network, before any published-port client can reach the clone.
pub trait RestoreHook {
    async fn run(&self) -> Result<()>;
}

/// No hook: the plan named none.
pub struct NoRestoreHook;

impl RestoreHook for NoRestoreHook {
    async fn run(&self) -> Result<()> {
        Ok(())
    }
}

/// How long the hook may take. It runs with ingress still gated, so every
/// second here is a second the clone stays unpublished.
const RESTORE_HOOK_BUDGET: std::time::Duration = std::time::Duration::from_secs(60);

/// How many seconds the hook's own `timeout` gives the command inside the
/// container, and how many more before it is killed outright. The budget above
/// is the client-side backstop and must exceed their sum.
const RESTORE_HOOK_COMMAND_SECS: u64 = 45;
const RESTORE_HOOK_KILL_AFTER_SECS: u64 = 5;

/// How much of the hook's stdout and of its stderr is kept, for the log line
/// and the error. The rest is drained and dropped, so a chatty hook neither
/// blocks on a full pipe nor grows the guest's memory.
const RESTORE_HOOK_OUTPUT_LIMIT: u64 = 64 * 1024;

/// What a bounded run leaves behind.
#[derive(Debug)]
struct BoundedOutput {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Run `argv` with a budget. The child is killed when the budget ends (and when
/// this future is dropped), and at most `RESTORE_HOOK_OUTPUT_LIMIT` bytes of each
/// stream are kept.
async fn run_bounded(argv: &[String], budget: std::time::Duration) -> Result<BoundedOutput> {
    use tokio::io::AsyncReadExt;
    let mut child = tokio::process::Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawning {:?}", argv[0]))?;
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let run = async {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let (read_out, read_err) = tokio::join!(
            async {
                (&mut stdout)
                    .take(RESTORE_HOOK_OUTPUT_LIMIT)
                    .read_to_end(&mut out)
                    .await?;
                tokio::io::copy(&mut stdout, &mut tokio::io::sink()).await
            },
            async {
                (&mut stderr)
                    .take(RESTORE_HOOK_OUTPUT_LIMIT)
                    .read_to_end(&mut err)
                    .await?;
                tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await
            }
        );
        read_out.context("reading the hook's stdout")?;
        read_err.context("reading the hook's stderr")?;
        let status = child.wait().await.context("waiting for the hook")?;
        Ok::<_, anyhow::Error>(BoundedOutput {
            status,
            stdout: out,
            stderr: err,
        })
    };
    match tokio::time::timeout(budget, run).await {
        Ok(result) => result,
        Err(_) => {
            // The future above is dropped by the timeout; kill the child and reap it,
            // so nothing it spawned keeps running under the clone's name.
            let _ = child.start_kill();
            let _ = child.wait().await;
            bail!("did not finish within {budget:?}")
        }
    }
}

/// The plan's hook, run inside the container through `podman exec`, as the
/// container's user, under the container's own `timeout` so the command ends
/// there when the budget does, not only its client here.
pub struct ContainerRestoreHook {
    pub command: String,
}

/// Whether the plan's hook has a running container to run in, read from podman
/// at publication. A pre-start snapshot restores before the container exists;
/// a snapshot taken while podman was still starting it finds it not running.
/// Neither has anything stale to redo: the container that starts after the
/// restore starts fresh.
#[derive(Debug, PartialEq, Eq)]
enum HookEligibility {
    Run,
    Skip(String),
    /// podman could not say: the restore fails closed rather than publish a
    /// clone whose stale-state repair was silently left out.
    Fail(String),
}

/// Decide from `podman inspect --format {{.State.Running}} fcvm-container`:
/// `true` runs the hook; `false` skips it (a snapshot taken while podman was
/// still starting the container); podman's "no such container" skips it (a
/// pre-start snapshot); any other failure fails the restore.
fn hook_eligibility(inspect: &BoundedOutput) -> HookEligibility {
    let stdout = String::from_utf8_lossy(&inspect.stdout);
    let state = stdout.trim();
    if inspect.status.success() && state == "true" {
        return HookEligibility::Run;
    }
    if inspect.status.success() {
        return HookEligibility::Skip(format!("fcvm-container is not running (Running={state})"));
    }
    let stderr = String::from_utf8_lossy(&inspect.stderr);
    let detail = format!(
        "podman inspect exited {}: {}",
        inspect.status,
        stderr.trim()
    );
    if stderr.contains("no such container") || stderr.contains("no container with name or ID") {
        HookEligibility::Skip(detail)
    } else {
        HookEligibility::Fail(detail)
    }
}

/// How long the container-state read may take. A read that does not finish in
/// time fails the restore closed, like any other inspect failure.
const HOOK_INSPECT_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);

impl ContainerRestoreHook {
    fn inspect_argv() -> Vec<String> {
        let mut argv: Vec<String> = crate::container::podman_cmd_prefix().to_vec();
        argv.extend(
            [
                "podman",
                "inspect",
                "--format",
                "{{.State.Running}}",
                "fcvm-container",
            ]
            .into_iter()
            .map(str::to_string),
        );
        argv
    }

    fn argv(&self) -> Vec<String> {
        let mut argv: Vec<String> = crate::container::podman_cmd_prefix().to_vec();
        argv.extend(
            ["podman", "exec", "fcvm-container", "timeout", "-k"]
                .into_iter()
                .map(str::to_string),
        );
        argv.push(RESTORE_HOOK_KILL_AFTER_SECS.to_string());
        argv.push(RESTORE_HOOK_COMMAND_SECS.to_string());
        argv.extend(["sh", "-c"].into_iter().map(str::to_string));
        argv.push(self.command.clone());
        argv
    }
}

impl RestoreHook for ContainerRestoreHook {
    async fn run(&self) -> Result<()> {
        let inspect = run_bounded(&Self::inspect_argv(), HOOK_INSPECT_BUDGET)
            .await
            .context("reading fcvm-container's state before the restore hook")?;
        match hook_eligibility(&inspect) {
            HookEligibility::Run => {}
            HookEligibility::Skip(reason) => {
                eprintln!("[fc-agent] restore hook skipped: {reason}");
                return Ok(());
            }
            HookEligibility::Fail(reason) => {
                bail!(
                    "restore hook {:?} cannot tell whether the container runs; {reason}",
                    self.command
                );
            }
        }
        let started = std::time::Instant::now();
        let output = run_bounded(&self.argv(), RESTORE_HOOK_BUDGET)
            .await
            .with_context(|| format!("restore hook {:?}", self.command))?;
        let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !output.status.success() {
            bail!(
                "restore hook {:?} exited {} after {elapsed_ms:.0} ms; stdout: {}; stderr: {}",
                self.command,
                output.status,
                stdout.trim(),
                stderr.trim()
            );
        }
        eprintln!(
            "[fc-agent] restore hook {:?} exited 0 in {elapsed_ms:.0} ms; stdout: {}",
            self.command,
            stdout.trim()
        );
        Ok(())
    }
}

/// A restored clone's network between the two phases of its boundary: the
/// snapshot-time sockets are retired and egress is open, ingress is still
/// gated, and the guest-wide boundary lock is held until `publish` opens it.
/// The restore sequence does its other work (the clock step, the journald
/// restart, the NFS remount, the exec and egress-proxy waits) in between, so
/// the hook runs with everything a workload may need already restored.
pub struct RestoredNetwork {
    _transaction_lock: std::fs::File,
    gate: IptablesGate<SystemCommandRunner>,
    report: RestoreNetworkReport,
}

impl RestoredNetwork {
    /// What the boundary did so far (the publication fields are still zero).
    pub fn report(&self) -> &RestoreNetworkReport {
        &self.report
    }

    /// Run the plan's restore hook, if any, then open ingress: the clone is
    /// published. A hook failure keeps it unpublished and fails the restore.
    pub async fn publish(self, hook: Option<&str>) -> Result<RestoreNetworkReport> {
        let report = match hook {
            Some(command) => {
                let hook = ContainerRestoreHook {
                    command: command.to_string(),
                };
                publish_with(&self.gate, self.report, &hook).await?
            }
            None => publish_with(&self.gate, self.report, &NoRestoreHook).await?,
        };
        eprintln!(
            "[fc-agent] snapshot network published: destroyed={} already_gone={} \
             verified_armed={} link=UNTOUCHED gate=open",
            report.tally.destroyed, report.tally.already_gone, report.verified_armed
        );
        Ok(report)
    }
}

pub async fn restore_snapshot_network() -> Result<RestoredNetwork> {
    let transaction_lock = acquire_boundary_lock()?;
    let gate = IptablesGate::new(SystemCommandRunner);
    let receive_barrier = SystemPacketReceiveBarrier;
    let mut diagnostic = SystemSocketDiagnostic::new();
    let store = FileManifestStore::default();
    let report = restore_boundary_with(
        &gate,
        &receive_barrier,
        &mut diagnostic,
        &store,
        CLEANUP_BUDGET,
    )
    .await?;
    eprintln!(
        "[fc-agent] snapshot network cleanup complete: destroyed={} already_gone={} \
         verified_armed={} link=UNTOUCHED gate=egress-open",
        report.tally.destroyed, report.tally.already_gone, report.verified_armed
    );
    Ok(RestoredNetwork {
        _transaction_lock: transaction_lock,
        gate,
        report,
    })
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
    use std::collections::VecDeque;
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
        /// The ingress half of the gate (new inbound connections rejected).
        gate_closed: bool,
        /// The egress half of the gate (new outbound connections rejected).
        /// Closed with the ingress half; opened by `open_egress` alone.
        egress_closed: bool,
        /// The restored clone's UDP rejection, part of the ingress half.
        udp_gated: bool,
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
        fail_hook: bool,
        /// Calls to the model diagnostic's destroy, one datagram each.
        destroy_calls: usize,
    }

    #[derive(Clone)]
    struct ModelGate(Arc<Mutex<ModelState>>);

    impl FirewallGate for ModelGate {
        async fn close(&self) -> Result<()> {
            let mut state = self.0.lock().unwrap();
            state.gate_closed = true;
            state.egress_closed = true;
            state.events.push("gate-close");
            if state.fail_gate_close {
                bail!("injected gate close failure after partial installation");
            }
            Ok(())
        }

        async fn open_egress(&self) -> Result<()> {
            let mut state = self.0.lock().unwrap();
            state.egress_closed = false;
            state.events.push("gate-open-egress");
            Ok(())
        }

        async fn open(&self) -> Result<()> {
            let mut state = self.0.lock().unwrap();
            state.gate_closed = false;
            state.egress_closed = false;
            state.udp_gated = false;
            state.events.push("gate-open");
            Ok(())
        }

        async fn gate_udp_ingress(&self) -> Result<()> {
            let mut state = self.0.lock().unwrap();
            assert!(
                state.gate_closed,
                "the UDP rejection was added to a gate that is not closed"
            );
            state.udp_gated = true;
            state.events.push("gate-udp-ingress");
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

    /// A restore hook that records when it ran, so a test can place it against the
    /// gate opening.
    struct ModelHook(Arc<Mutex<ModelState>>);

    impl RestoreHook for ModelHook {
        async fn run(&self) -> Result<()> {
            let mut state = self.0.lock().unwrap();
            assert!(
                !state.egress_closed,
                "the restore hook ran behind a closed egress gate: its own connections, \
                 and the workload's reconnects, would be answered with resets"
            );
            assert!(
                state.gate_closed && state.udp_gated,
                "the restore hook ran after the clone was published, or without the UDP rejection"
            );
            state.events.push("restore-hook");
            if state.fail_hook {
                bail!("injected restore hook failure");
            }
            Ok(())
        }
    }

    struct ModelBarrier(Arc<Mutex<ModelState>>);

    impl PacketReceiveBarrier for ModelBarrier {
        fn synchronize(&self) -> Result<()> {
            let mut state = self.0.lock().unwrap();
            assert!(
                state.gate_closed && state.egress_closed,
                "receive barrier ran before the NEW-flow gate"
            );
            // The link is deliberately never touched: the gate already rejects
            // NEW flows, and downing the link additionally purged the device's
            // routes and neighbours. Both purges reached production as silent
            // failures (ee3f5c30; the 2026-08-15 published-port flake). A
            // boundary that starts downing links again fails here.
            assert!(
                !state.link_down,
                "the snapshot boundary took the link down: it must not disturb \
                 the VM being snapshotted"
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
                state.gate_closed && state.egress_closed,
                "cookie dump raced ahead of the NEW-flow gate"
            );
            assert!(
                state.events.contains(&"rx-barrier"),
                "cookie dump raced ahead of the receive barrier"
            );
            assert!(
                !state.link_down,
                "the snapshot boundary took the link down: it must not disturb \
                 the VM being snapshotted"
            );
            state.events.push("cookie-dump");
            if state.fail_dump {
                bail!("injected socket dump failure");
            }
            Ok(state.live.clone())
        }

        fn destroy(&mut self, sockets: &[TcpSocketIdentity]) -> Result<Vec<DestroyOutcome>> {
            let mut state = self.0.lock().unwrap();
            state.destroy_calls += 1;
            assert!(
                state.gate_closed,
                "restore published ingress before destroying sockets"
            );
            assert!(
                !state.egress_closed,
                "restore retired a socket with egress still closed: the daemon that owned it \
                 reconnects into a reset"
            );
            // The closed gate is what protects the cleanup; the link stays up
            // throughout so the clone's routes and neighbours survive.
            assert!(
                !state.link_down,
                "restore took the link down: the boundary must not purge the \
                 clone's routes and neighbours"
            );
            let mut outcomes = Vec::with_capacity(sockets.len());
            for socket in sockets {
                state.events.push("cookie-destroy");
                if state.fail_destroy {
                    bail!("injected cookie destroy failure");
                }
                if let Some(index) = state.live.iter().position(|candidate| candidate == socket) {
                    state.live.remove(index);
                    outcomes.push(DestroyOutcome::Destroyed);
                } else {
                    outcomes.push(DestroyOutcome::AlreadyGone);
                }
            }
            Ok(outcomes)
        }

        fn destroy_datagrams(&self) -> usize {
            self.0.lock().unwrap().destroy_calls
        }
    }

    /// How the model kernel answers each SOCK_DESTROY it is sent.
    #[derive(Clone, Copy, Debug)]
    enum ModelAnswer {
        /// What sock_diag does: 0 for a live cookie, -ENOENT for any other.
        Kernel,
        /// An error acknowledgement with this errno for every request.
        Errno(i32),
        /// The socket's diag record, which is what a dump displays and is no
        /// proof that anything was destroyed.
        Displayed,
        /// An acknowledgement that echoes a request of another type.
        WrongRequest,
        /// No reply at all, as when the receive timeout expires.
        Silence,
    }

    /// How the model kernel hands its queued replies to the receiver.
    #[derive(Clone, Copy, Debug)]
    enum ModelDelivery {
        /// One reply per datagram, in request order, as netlink_ack queues them.
        InOrder,
        /// One reply per datagram, last request first.
        Reversed,
        /// Every reply of a send packed into one datagram.
        Packed,
    }

    /// The kernel's side of a NETLINK_SOCK_DIAG socket, as sock_diag_rcv
    /// behaves: every request in a datagram is handled during the send, and one
    /// reply per request is queued for the receiver. It counts what the
    /// destroyer asked of it.
    struct ModelKernel {
        live: Vec<[u32; 2]>,
        answer: ModelAnswer,
        delivery: ModelDelivery,
        /// Sockets opened.
        opens: usize,
        /// Requests carried by each datagram sent.
        datagrams: Vec<usize>,
        receives: usize,
        queue: VecDeque<Vec<u8>>,
        /// Most datagrams queued unread at once.
        most_queued: usize,
        /// Bytes the socket's receive buffer holds.
        receive_capacity: usize,
        /// Whether SO_RCVBUFFORCE succeeds, as it does with CAP_NET_ADMIN.
        can_force: bool,
        queued_bytes: usize,
        /// Replies dropped because the receive buffer was full.
        dropped: usize,
        /// A drop that the next receive reports as ENOBUFS.
        overrun: bool,
    }

    /// What one acknowledgement charges to a receive buffer in the 6.18 guest
    /// kernel, where 256 of them fill the default 212,992 bytes.
    const MODEL_ACK_CHARGE: usize = 832;

    impl ModelKernel {
        fn reply(&mut self, header: NetlinkHeader, payload: &[u8]) -> Result<Option<Vec<u8>>> {
            let request: InetDiagRequest = read_unaligned(payload)?;
            let errno = match self.answer {
                ModelAnswer::Silence => return Ok(None),
                ModelAnswer::Displayed => return Ok(Some(displayed_record(header, &request))),
                ModelAnswer::WrongRequest => {
                    let echoed = NetlinkHeader {
                        message_type: SOCK_DIAG_BY_FAMILY,
                        ..header
                    };
                    return Ok(Some(error_acknowledgement(echoed, payload, 0)));
                }
                ModelAnswer::Errno(errno) => errno,
                ModelAnswer::Kernel => {
                    match self
                        .live
                        .iter()
                        .position(|cookie| *cookie == request.id.cookie)
                    {
                        Some(index) => {
                            self.live.remove(index);
                            0
                        }
                        None => libc::ENOENT,
                    }
                }
            };
            Ok(Some(error_acknowledgement(header, payload, -errno)))
        }
    }

    /// An NLMSG_ERROR as netlink_ack builds it: the error, the request's
    /// header, and for a failure the request's payload as well.
    fn error_acknowledgement(request: NetlinkHeader, payload: &[u8], error: i32) -> Vec<u8> {
        const NLM_F_CAPPED: u16 = 0x100;
        let echoed: &[u8] = if error == 0 { &[] } else { payload };
        let mut bytes = Vec::new();
        append_netlink_header(
            &mut bytes,
            NetlinkHeader {
                length: (2 * std::mem::size_of::<NetlinkHeader>()
                    + std::mem::size_of::<i32>()
                    + echoed.len()) as u32,
                message_type: NLMSG_ERROR,
                flags: if error == 0 { NLM_F_CAPPED } else { 0 },
                sequence: request.sequence,
                port_id: 0,
            },
        );
        bytes.extend_from_slice(&error.to_ne_bytes());
        append_netlink_header(&mut bytes, request);
        bytes.extend_from_slice(echoed);
        bytes
    }

    fn displayed_record(request: NetlinkHeader, diag: &InetDiagRequest) -> Vec<u8> {
        let mut bytes = Vec::new();
        append_netlink_header(
            &mut bytes,
            NetlinkHeader {
                length: (std::mem::size_of::<NetlinkHeader>()
                    + std::mem::size_of::<InetDiagMessage>()) as u32,
                message_type: SOCK_DIAG_BY_FAMILY,
                flags: 0,
                sequence: request.sequence,
                port_id: 0,
            },
        );
        append_diag_message(
            &mut bytes,
            InetDiagMessage {
                family: diag.family,
                state: 1,
                timer: 0,
                retransmits: 0,
                id: diag.id,
                expires: 0,
                receive_queue: 0,
                write_queue: 0,
                uid: 0,
                inode: 1,
            },
        );
        bytes
    }

    #[derive(Clone)]
    struct SharedKernel(Arc<Mutex<ModelKernel>>);

    impl SharedKernel {
        fn new(live: &[TcpSocketIdentity], answer: ModelAnswer, delivery: ModelDelivery) -> Self {
            Self(Arc::new(Mutex::new(ModelKernel {
                live: live.iter().map(|socket| socket.id.cookie).collect(),
                answer,
                delivery,
                opens: 0,
                datagrams: Vec::new(),
                receives: 0,
                queue: VecDeque::new(),
                most_queued: 0,
                receive_capacity: 212_992,
                can_force: true,
                queued_bytes: 0,
                dropped: 0,
                overrun: false,
            })))
        }

        fn opener(&self) -> impl FnMut() -> Result<ModelChannel> {
            let kernel = self.clone();
            move || {
                kernel.0.lock().unwrap().opens += 1;
                Ok(ModelChannel {
                    kernel: kernel.clone(),
                    current: Vec::new(),
                })
            }
        }
    }

    struct ModelChannel {
        kernel: SharedKernel,
        current: Vec<u8>,
    }

    impl NetlinkChannel for ModelChannel {
        fn send(&mut self, datagram: &[u8]) -> Result<()> {
            let mut kernel = self.kernel.0.lock().unwrap();
            let mut requests = 0;
            let mut replies = Vec::new();
            for_each_netlink_message(datagram, |header, payload| {
                assert_eq!(header.message_type, SOCK_DESTROY);
                assert_eq!(header.flags, NLM_F_REQUEST | NLM_F_ACK);
                requests += 1;
                replies.extend(kernel.reply(header, payload)?);
                Ok(false)
            })?;
            kernel.datagrams.push(requests);
            let datagrams = match kernel.delivery {
                ModelDelivery::InOrder => replies,
                ModelDelivery::Reversed => replies.into_iter().rev().collect(),
                ModelDelivery::Packed if replies.is_empty() => Vec::new(),
                ModelDelivery::Packed => vec![replies.concat()],
            };
            for datagram in datagrams {
                // A reply that does not fit the receive buffer is dropped, and
                // the next receive fails with ENOBUFS.
                if kernel.queued_bytes + MODEL_ACK_CHARGE > kernel.receive_capacity {
                    kernel.dropped += 1;
                    kernel.overrun = true;
                    continue;
                }
                kernel.queued_bytes += MODEL_ACK_CHARGE;
                kernel.queue.push_back(datagram);
            }
            kernel.most_queued = kernel.most_queued.max(kernel.queue.len());
            Ok(())
        }

        fn receive(&mut self) -> Result<&[u8]> {
            let mut kernel = self.kernel.0.lock().unwrap();
            kernel.receives += 1;
            if kernel.overrun {
                kernel.overrun = false;
                return Err(io::Error::from_raw_os_error(libc::ENOBUFS))
                    .context("receiving SOCK_DIAG netlink response");
            }
            // An empty queue is what the socket's receive timeout reports.
            self.current = kernel
                .queue
                .pop_front()
                .context("receiving SOCK_DIAG netlink response: timed out")?;
            kernel.queued_bytes -= MODEL_ACK_CHARGE;
            Ok(&self.current)
        }

        fn reserve_receive_buffer(&mut self, bytes: usize) -> Result<usize> {
            let mut kernel = self.kernel.0.lock().unwrap();
            if kernel.can_force {
                // The kernel doubles the value to cover its own overhead.
                kernel.receive_capacity = bytes * 2;
            }
            Ok(kernel.receive_capacity)
        }
    }

    /// The restore path over the real destroyer, with the model kernel behind it.
    struct ChannelDiag<F>(CookieDestroyer<ModelChannel, F>);

    impl<F: FnMut() -> Result<ModelChannel>> SocketDiagnostic for ChannelDiag<F> {
        fn dump_tcp_sockets(&mut self) -> Result<Vec<TcpSocketIdentity>> {
            bail!("a restore never dumps sockets")
        }

        fn destroy(&mut self, sockets: &[TcpSocketIdentity]) -> Result<Vec<DestroyOutcome>> {
            self.0.destroy(sockets)
        }

        fn destroy_datagrams(&self) -> usize {
            self.0.datagrams
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
            record_input(&self.0, program, args, input);
            Ok(std::process::Output {
                status: std::process::ExitStatus::from_raw(0),
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }

    /// Records a command fed on stdin as the command line, then one
    /// `<program>< <line>` entry per input line.
    fn record_input(calls: &Mutex<Vec<String>>, program: &str, args: &[&str], input: &[u8]) {
        let mut calls = calls.lock().unwrap();
        calls.push(format!("{program} {}", args.join(" ")));
        for line in String::from_utf8_lossy(input).lines() {
            calls.push(format!("{program}< {line}"));
        }
    }

    /// A restored clone is behind by the snapshot's age and chronyd steps at any
    /// update, so an NTP reply while egress is gated jumps the clock into the
    /// window the restore keeps its own step out of (issue 1242). The gate the
    /// snapshot carries rejects the guest's NTP requests in both families, and
    /// no other outbound UDP, so DNS keeps working.
    #[tokio::test]
    async fn the_egress_gate_rejects_ntp_and_no_other_udp() {
        let runner = RecordingRunner::default();
        let calls = runner.0.clone();
        IptablesGate::new(runner).close().await.unwrap();
        let calls = calls.lock().unwrap().join("\n");
        // The whole OUTPUT chain, so a rule that gates UDP without naming it (a
        // protocol-less REJECT) fails here too.
        for (program, loopback, unreachable) in [
            ("iptables", "127.0.0.0/8", "icmp-port-unreachable"),
            ("ip6tables", "::1/128", "icmp6-port-unreachable"),
        ] {
            let prefix = format!("{program}-restore< -A FCVM_SNAPSHOT_OUT ");
            let appended: Vec<&str> = calls
                .lines()
                .filter_map(|call| call.strip_prefix(prefix.as_str()))
                .collect();
            let ntp = format!("-p udp -m udp --dport 123 -j REJECT --reject-with {unreachable}");
            let local = format!("-p tcp -d {loopback} -j RETURN");
            assert_eq!(
                appended,
                [
                    local.as_str(),
                    ntp.as_str(),
                    "-p tcp -m conntrack --ctstate NEW -j REJECT --reject-with tcp-reset",
                    "-j RETURN",
                ],
                "{program}'s OUTPUT gate chain:\n{calls}"
            );
        }
        assert!(
            !calls
                .lines()
                .any(|call| call.contains("FCVM_SNAPSHOT_IN") && call.contains("-p udp")),
            "the snapshot side gates UDP ingress:\n{calls}"
        );
    }

    /// The source's gate rejected any poll chronyd made during the cookie dump,
    /// so the source asks chronyd to measure once its gate has reopened.
    #[test]
    fn the_source_asks_chronyd_to_measure_after_its_gate_reopens() {
        let source = include_str!("snapshot_network.rs");
        let start = source
            .find("pub async fn resume_source_network(")
            .expect("resume_source_network is in snapshot_network.rs");
        let len = source[start..]
            .find("\n}\n")
            .expect("resume_source_network has a closing brace");
        let body = &source[start..start + len];
        let reopened = body
            .find("source VM snapshot network reopened")
            .expect("the source no longer reports its reopened gate");
        let burst = body
            .find("tokio::spawn(chrony_burst())")
            .expect("the source no longer asks chronyd to measure after its gate reopens");
        assert!(reopened < burst, "reopened={reopened} burst={burst}");
    }

    #[tokio::test]
    async fn gateway_address_cannot_bypass_directional_new_flow_gate() {
        let runner = RecordingRunner::default();
        let calls = runner.0.clone();
        IptablesGate::new(runner).close().await.unwrap();
        let calls = calls.lock().unwrap().join("\n");

        assert!(
            calls.contains("iptables-restore< -A FCVM_SNAPSHOT_IN -p tcp -s 127.0.0.0/8 -j RETURN")
        );
        assert!(calls
            .contains("iptables-restore< -A FCVM_SNAPSHOT_OUT -p tcp -d 127.0.0.0/8 -j RETURN"));
        assert!(calls.contains(
            "iptables-restore< -A FCVM_SNAPSHOT_IN -p tcp -m conntrack --ctstate NEW -j REJECT"
        ));
        assert!(calls.contains(
            "iptables-restore< -A FCVM_SNAPSHOT_OUT -p tcp -m conntrack --ctstate NEW -j REJECT"
        ));
        assert!(
            !calls.contains("10.0.2.") && !calls.contains("fd00:"),
            "gateway exemptions let routed external clients cross the generation boundary:\n{calls}"
        );
        // A published-port SYN is DNATed to 127.0.0.1 in PREROUTING, before filter
        // INPUT, so a destination-loopback exemption there would let it through
        // while the cookie dump or the restore hook runs. Locally redirected
        // traffic (the egress proxy's) enters on lo; that is the exemption.
        assert!(
            !calls.contains("FCVM_SNAPSHOT_IN -p tcp -d 127.0.0.0/8")
                && !calls.contains("FCVM_SNAPSHOT_IN -p tcp -d ::1/128"),
            "a destination-loopback exemption on INPUT lets DNATed published-port SYNs \
             cross the gate:\n{calls}"
        );
        assert!(
            calls.contains("iptables-restore< -A FCVM_SNAPSHOT_IN -i lo -j RETURN")
                && calls.contains("ip6tables-restore< -A FCVM_SNAPSHOT_IN -i lo -j RETURN"),
            "the egress proxy's redirected flows (entering on lo) must pass INPUT:\n{calls}"
        );
        // The UDP ingress rejection belongs to the restored clone only (see
        // the_udp_rejection_is_installed_on_restore_only): installed by close(), it
        // would answer a live client's datagram on the VM being snapshotted with an
        // ICMP unreachable during the cookie dump, and a snapshot must not disturb
        // its source. The NTP egress rule is the source's own poll, which chronyd
        // retries (the_egress_gate_rejects_ntp_and_no_other_udp).
        assert!(
            !calls
                .lines()
                .any(|call| call.contains("FCVM_SNAPSHOT_IN") && call.contains("-p udp")),
            "close() gates UDP ingress on the source VM during the cookie dump:\n{calls}"
        );
    }

    /// The restored clone also refuses client-direction UDP while its ingress is
    /// gated: a published UDP port reaches the workload like a TCP one, and a client
    /// whose flow predates the snapshot is ESTABLISHED in the restored guest, so the
    /// match is the flow's direction, not its state. The rule is inserted after the
    /// inherited gate is verified, below the loopback exemptions, and the ingress
    /// open removes it with the chain. The snapshot side never installs it.
    ///
    /// RED BEFORE THE FIX: the rule was part of every close(), the source VM's too.
    #[tokio::test]
    async fn the_udp_rejection_is_installed_on_restore_only() {
        let runner = RecordingRunner::default();
        let calls = runner.0.clone();
        IptablesGate::new(runner).gate_udp_ingress().await.unwrap();
        let calls = calls.lock().unwrap().join("\n");
        assert!(
            calls.contains(
                "iptables -w -I FCVM_SNAPSHOT_IN 3 -p udp -m conntrack --ctdir ORIGINAL -j REJECT \
                 --reject-with icmp-port-unreachable"
            ) && calls.contains(
                "ip6tables -w -I FCVM_SNAPSHOT_IN 3 -p udp -m conntrack --ctdir ORIGINAL -j REJECT \
                 --reject-with icmp6-port-unreachable"
            ),
            "client-direction UDP passes the restored clone's closed ingress gate:\n{calls}"
        );
        assert!(
            !calls.contains("--ctstate NEW") && !calls.contains("FCVM_SNAPSHOT_OUT"),
            "the UDP rule must match direction, not state, and never gate egress:\n{calls}"
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
            record_input(&self.calls, program, args, input);
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
        -A FCVM_SNAPSHOT_IN -i lo -j RETURN\n\
        -A FCVM_SNAPSHOT_IN -p tcp -m conntrack --ctstate NEW -j REJECT --reject-with tcp-reset\n\
        -A FCVM_SNAPSHOT_IN -j RETURN\n\
        -A FCVM_SNAPSHOT_OUT -d 127.0.0.0/8 -p tcp -j RETURN\n\
        -A FCVM_SNAPSHOT_OUT -p udp -m udp --dport 123 -j REJECT --reject-with icmp-port-unreachable\n\
        -A FCVM_SNAPSHOT_OUT -p tcp -m conntrack --ctstate NEW -j REJECT --reject-with tcp-reset\n\
        -A FCVM_SNAPSHOT_OUT -j RETURN\n";

    /// Same capture for the IPv6 family (`-d ::1/128 -p tcp -j RETURN` etc.).
    /// The NTP lines of both dumps are iptables 1.8.10's rendering, legacy and
    /// nf_tables alike, of the rule as `gate_chain_rules` appends it (with
    /// `-m udp`); the rest of each dump is the capture described above.
    const ARMED_V6_DUMP: &str = "-P INPUT ACCEPT\n\
        -P FORWARD ACCEPT\n\
        -P OUTPUT ACCEPT\n\
        -N FCVM_SNAPSHOT_IN\n\
        -N FCVM_SNAPSHOT_OUT\n\
        -A INPUT -j FCVM_SNAPSHOT_IN\n\
        -A OUTPUT -j FCVM_SNAPSHOT_OUT\n\
        -A FCVM_SNAPSHOT_IN -s ::1/128 -p tcp -j RETURN\n\
        -A FCVM_SNAPSHOT_IN -i lo -j RETURN\n\
        -A FCVM_SNAPSHOT_IN -p tcp -m conntrack --ctstate NEW -j REJECT --reject-with tcp-reset\n\
        -A FCVM_SNAPSHOT_IN -j RETURN\n\
        -A FCVM_SNAPSHOT_OUT -d ::1/128 -p tcp -j RETURN\n\
        -A FCVM_SNAPSHOT_OUT -p udp -m udp --dport 123 -j REJECT --reject-with icmp6-port-unreachable\n\
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

    /// One family's filter table, as far as `close()` touches it: the gate
    /// chains and the built-in INPUT and OUTPUT chains, each rule as tokens.
    #[derive(Clone, Default)]
    struct ModelTable {
        chains: std::collections::BTreeMap<String, Vec<Vec<String>>>,
        input: Vec<Vec<String>>,
        output: Vec<Vec<String>>,
    }

    impl ModelTable {
        fn builtin(&mut self, chain: &str) -> Option<&mut Vec<Vec<String>>> {
            match chain {
                "INPUT" => Some(&mut self.input),
                "OUTPUT" => Some(&mut self.output),
                _ => None,
            }
        }

        fn append(&mut self, chain: &str, rule: Vec<String>) -> bool {
            if let Some(rules) = self.builtin(chain) {
                rules.push(rule);
                return true;
            }
            match self.chains.get_mut(chain) {
                Some(rules) => {
                    rules.push(rule);
                    true
                }
                None => false,
            }
        }

        /// The armed gate of one family, as a snapshot carries it.
        fn armed(loopback_network: &str) -> Self {
            let mut table = Self::default();
            for (hook, chain, flag) in [
                ("INPUT", GATE_INPUT_CHAIN, "-s"),
                ("OUTPUT", GATE_OUTPUT_CHAIN, "-d"),
            ] {
                table
                    .chains
                    .insert(chain.to_string(), gate_chain_rules(flag, loopback_network));
                let jump = hook_jump_args(chain).map(str::to_string).to_vec();
                table.builtin(hook).unwrap().push(jump);
            }
            table
        }
    }

    /// iptables and iptables-restore over one model table per family. After
    /// every command that changes a table, it records each gate chain that is
    /// hooked from INPUT or OUTPUT and does not hold every rule of the gate:
    /// a moment in which that chain admits what the gate exists to reject.
    #[derive(Clone)]
    struct ModelIptables {
        tables: Arc<Mutex<[ModelTable; 2]>>,
        gaps: Arc<Mutex<Vec<String>>>,
    }

    impl ModelIptables {
        fn new(ipv4: ModelTable, ipv6: ModelTable) -> Self {
            Self {
                tables: Arc::new(Mutex::new([ipv4, ipv6])),
                gaps: Arc::default(),
            }
        }

        fn check(&self, family: Family, command: &str) {
            let tables = self.tables.lock().unwrap();
            let table = &tables[family_index(family)];
            let network = match family {
                Family::Ipv4 => IPV4_LOOPBACK,
                Family::Ipv6 => IPV6_LOOPBACK,
            };
            for (hooks, chain, flag) in [
                (&table.input, GATE_INPUT_CHAIN, "-s"),
                (&table.output, GATE_OUTPUT_CHAIN, "-d"),
            ] {
                let jump = hook_jump_args(chain);
                if !hooks.iter().any(|rule| rule == &jump) {
                    continue;
                }
                let held: Vec<Vec<String>> = table
                    .chains
                    .get(chain)
                    .map(|rules| rules.iter().map(|rule| sorted_tokens(rule)).collect())
                    .unwrap_or_default();
                let complete = gate_chain_rules(flag, network)
                    .iter()
                    .all(|rule| held.contains(&sorted_tokens(rule)));
                if !complete {
                    self.gaps.lock().unwrap().push(format!(
                        "after `{command}` the hooked {} chain {chain} holds {} rules",
                        family.name(),
                        held.len()
                    ));
                }
            }
        }

        fn exit(code: i32) -> io::Result<std::process::Output> {
            Ok(std::process::Output {
                status: std::process::ExitStatus::from_raw(code << 8),
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }

    impl CommandRunner for ModelIptables {
        async fn output(&self, program: &str, args: &[&str]) -> io::Result<std::process::Output> {
            let family = family_of(program);
            let command = format!("{program} {}", args.join(" "));
            let ok = {
                let mut tables = self.tables.lock().unwrap();
                let table = &mut tables[family_index(family)];
                let owned = |tokens: &[&str]| tokens.iter().map(|t| t.to_string()).collect();
                match args {
                    ["-w", "-L", chain] => table.chains.contains_key(*chain),
                    ["-w", "-N", chain] => {
                        table.chains.insert(chain.to_string(), Vec::new()).is_none()
                    }
                    ["-w", "-F", chain] => match table.chains.get_mut(*chain) {
                        Some(rules) => {
                            rules.clear();
                            true
                        }
                        None => false,
                    },
                    ["-w", "-A", chain, rule @ ..] => table.append(chain, owned(rule)),
                    ["-w", "-I", hook, "1", rule @ ..] => match table.builtin(hook) {
                        Some(rules) => {
                            rules.insert(0, owned(rule));
                            true
                        }
                        None => false,
                    },
                    ["-w", "-C", hook, rule @ ..] => {
                        let rule: Vec<String> = owned(rule);
                        table
                            .builtin(hook)
                            .is_some_and(|rules| rules.contains(&rule))
                    }
                    _ => panic!("the iptables model does not know `{command}`"),
                }
            };
            self.check(family, &command);
            Self::exit(if ok { 0 } else { 1 })
        }

        async fn output_with_input(
            &self,
            program: &str,
            args: &[&str],
            input: &[u8],
        ) -> io::Result<std::process::Output> {
            let family = family_of(program);
            let command = format!("{program} {}", args.join(" "));
            assert!(
                program.ends_with("-restore") && args.contains(&"--noflush"),
                "without --noflush iptables-restore flushes the whole table: `{command}`"
            );
            let input = String::from_utf8_lossy(input).to_string();
            {
                let mut tables = self.tables.lock().unwrap();
                // Staged on a copy and committed whole at COMMIT, as
                // iptables-restore does on both backends.
                let mut staged = tables[family_index(family)].clone();
                let mut committed = false;
                for line in input.lines() {
                    let tokens: Vec<&str> = line.split_whitespace().collect();
                    match tokens.as_slice() {
                        ["*filter"] => {}
                        // With --noflush, declaring a chain flushes that chain
                        // only, creating it when it does not exist.
                        [declaration, "-", _counters] if declaration.starts_with(':') => {
                            staged
                                .chains
                                .insert(declaration[1..].to_string(), Vec::new());
                        }
                        ["-A", chain, rule @ ..] => {
                            let rule = rule.iter().map(|t| t.to_string()).collect();
                            if !staged.append(chain, rule) {
                                return Self::exit(1);
                            }
                        }
                        ["COMMIT"] => committed = true,
                        _ => panic!("the iptables-restore model does not know `{line}`"),
                    }
                }
                if !committed {
                    return Self::exit(1);
                }
                tables[family_index(family)] = staged;
            }
            self.check(family, &command);
            Self::exit(0)
        }
    }

    /// A restored clone whose inherited gate fails verification reinstalls
    /// it, over chains that are already hooked from INPUT and OUTPUT. Every
    /// moment in which a hooked chain holds less than the whole gate admits
    /// new flows the cookie manifest never saw, and the guest's NTP polls,
    /// while egress is gated. The reinstall replaces each chain in one
    /// transaction, so no such moment exists, on the restore side or on a
    /// fresh source.
    ///
    /// RED BEFORE THE FIX (issue 1244): close() flushed each chain with `-F`
    /// and refilled it one `-A` at a time, leaving a hooked chain empty from
    /// the flush to the first append and partial until the last.
    #[tokio::test]
    async fn reinstalling_the_gate_never_leaves_a_hooked_chain_incomplete() {
        // The inherited gate, tampered so it fails verification: a foreign
        // rule at the end of the INPUT gate chain.
        let tampered = |network: &str| {
            let mut table = ModelTable::armed(network);
            table
                .chains
                .get_mut(GATE_INPUT_CHAIN)
                .unwrap()
                .push(["-p", "udp", "-j", "ACCEPT"].map(str::to_string).to_vec());
            table
        };
        for (case, ipv4, ipv6) in [
            ("restore", tampered(IPV4_LOOPBACK), tampered(IPV6_LOOPBACK)),
            ("fresh", ModelTable::default(), ModelTable::default()),
        ] {
            let model = ModelIptables::new(ipv4, ipv6);
            IptablesGate::new(model.clone()).close().await.unwrap();
            let gaps = model.gaps.lock().unwrap().clone();
            assert!(gaps.is_empty(), "{case}: {gaps:#?}");
            let tables = model.tables.lock().unwrap();
            for (family, network) in [(Family::Ipv4, IPV4_LOOPBACK), (Family::Ipv6, IPV6_LOOPBACK)]
            {
                let table = &tables[family_index(family)];
                for (chain, flag) in [(GATE_INPUT_CHAIN, "-s"), (GATE_OUTPUT_CHAIN, "-d")] {
                    assert_eq!(
                        table.chains[chain],
                        gate_chain_rules(flag, network),
                        "{case}: {} {chain} after close()",
                        family.name()
                    );
                }
                assert_eq!(table.input[0], hook_jump_args(GATE_INPUT_CHAIN));
                assert_eq!(table.output[0], hook_jump_args(GATE_OUTPUT_CHAIN));
            }
        }
    }

    /// Opens the model nf_tables kernel for an `IptablesGate` under test.
    #[derive(Clone)]
    struct ModelNetfilter(
        Arc<Mutex<crate::nftables::model::Kernel>>,
        Arc<Mutex<Option<crate::nftables::model::Channel>>>,
    );

    impl ModelNetfilter {
        fn new(kernel: Arc<Mutex<crate::nftables::model::Kernel>>) -> Self {
            Self(kernel, Arc::new(Mutex::new(None)))
        }
    }

    impl NetfilterOpener for ModelNetfilter {
        type Channel = crate::nftables::model::Channel;

        fn take(&self) -> Result<Self::Channel> {
            if let Some(channel) = self.1.lock().unwrap().take() {
                return Ok(channel);
            }
            self.0.lock().unwrap().opens += 1;
            Ok(crate::nftables::model::Channel::new(self.0.clone()))
        }

        fn give_back(&self, channel: Self::Channel) {
            *self.1.lock().unwrap() = Some(channel);
        }
    }

    fn armed_model() -> Arc<Mutex<crate::nftables::model::Kernel>> {
        let mut kernel = crate::nftables::model::Kernel::default();
        kernel.arm(Family::Ipv4);
        kernel.arm(Family::Ipv6);
        Arc::new(Mutex::new(kernel))
    }

    fn holds_gate(table: &crate::nftables::model::Table, chain: &str) -> bool {
        use crate::nftables::model::Rule;
        table.chains.contains(&chain)
            || table
                .rules
                .iter()
                .any(|(c, _, rule)| *c == chain || matches!(rule, Rule::Jump(t) if *t == chain))
    }

    #[tokio::test]
    async fn opening_each_half_is_one_transaction_for_both_families() {
        // Each half of the gate opens in both families in ONE nf_tables
        // commit and spawns no process: egress (OUTPUT) first, ingress
        // (INPUT) second, two observable steps in that order.
        let kernel = armed_model();
        let runner = RecordingRunner::default();
        let calls = runner.0.clone();
        let gate = IptablesGate::with_netfilter(runner, ModelNetfilter::new(kernel.clone()));

        gate.open_egress().await.unwrap();
        {
            let k = kernel.lock().unwrap();
            assert_eq!(
                k.commits.len(),
                1,
                "egress must be one commit: {:?}",
                k.commits
            );
            let ops = k.commits[0].join("; ");
            assert!(
                ops.contains("ip delete rule OUTPUT") && ops.contains("ip6 delete rule OUTPUT"),
                "{ops}"
            );
            assert!(
                ops.contains("ip delete chain FCVM_SNAPSHOT_OUT")
                    && ops.contains("ip6 delete chain FCVM_SNAPSHOT_OUT"),
                "{ops}"
            );
            assert!(
                !ops.contains("INPUT") && !ops.contains("FCVM_SNAPSHOT_IN"),
                "egress touched ingress: {ops}"
            );
            for table in [&k.ipv4, &k.ipv6] {
                assert!(!holds_gate(table, GATE_OUTPUT_CHAIN), "{table:?}");
                assert!(
                    holds_gate(table, GATE_INPUT_CHAIN),
                    "ingress must stay gated: {table:?}"
                );
            }
        }

        gate.open().await.unwrap();
        let k = kernel.lock().unwrap();
        assert_eq!(
            k.commits.len(),
            2,
            "ingress must be one more commit: {:?}",
            k.commits
        );
        let ops = k.commits[1].join("; ");
        assert!(
            ops.contains("ip delete rule INPUT") && ops.contains("ip6 delete rule INPUT"),
            "{ops}"
        );
        for table in [&k.ipv4, &k.ipv6] {
            assert!(
                !holds_gate(table, GATE_INPUT_CHAIN) && !holds_gate(table, GATE_OUTPUT_CHAIN),
                "{table:?}"
            );
        }
        assert!(
            calls.lock().unwrap().is_empty(),
            "opening the gate must spawn no process: {:?}",
            calls.lock().unwrap()
        );
        let timings = gate.timings();
        assert!(
            timings.egress_commit_ms > 0.0 && timings.ingress_commit_ms > 0.0,
            "{timings:?}"
        );
    }

    #[tokio::test]
    async fn the_gate_socket_is_kept_and_never_released() {
        // Releasing an nfnetlink socket after a deletion waits an RCU grace
        // period while holding nf_tables' commit mutex: inline it delays the
        // caller, between the halves it delays the ingress commit, and on
        // another thread it lands in the exec of a process forked meanwhile.
        // The model's release blocks until let go (or two seconds pass).
        let kernel = armed_model();
        let (release, blocker) = std::sync::mpsc::channel::<()>();
        kernel.lock().unwrap().close_blocker = Some(Arc::new(Mutex::new(blocker)));
        let netfilter = ModelNetfilter::new(kernel.clone());
        let started = std::time::Instant::now();
        for _ in 0..2 {
            let gate = IptablesGate::with_netfilter(RecordingRunner::default(), netfilter.clone());
            gate.open_egress().await.unwrap();
            gate.open().await.unwrap();
            drop(gate);
            kernel.lock().unwrap().arm(Family::Ipv4);
            kernel.lock().unwrap().arm(Family::Ipv6);
        }
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "opening the gate twice waited {elapsed:?} for a socket release"
        );
        let k = kernel.lock().unwrap();
        assert_eq!(k.commits.len(), 4);
        assert_eq!(k.opens, 1, "every removal must use the one kept socket");
        assert_eq!(k.closes, 0, "the gate's socket was released");
        drop(k);
        drop(release);
    }

    #[tokio::test]
    async fn removal_deletes_every_observed_hook_and_nothing_else() {
        use crate::nftables::model::Rule;
        let kernel = armed_model();
        {
            let mut k = kernel.lock().unwrap();
            // A duplicate hook an interrupted close could leave, and a jump
            // that carries a match, which is not ours.
            k.add_rule(Family::Ipv4, "INPUT", Rule::Jump(GATE_INPUT_CHAIN));
            k.add_rule(Family::Ipv6, "INPUT", Rule::Other);
        }
        let gate = IptablesGate::with_netfilter(
            RecordingRunner::default(),
            ModelNetfilter::new(kernel.clone()),
        );
        gate.open().await.unwrap();
        let k = kernel.lock().unwrap();
        assert_eq!(k.commits.len(), 1);
        assert_eq!(
            k.commits[0]
                .iter()
                .filter(|op| op.as_str() == "ip delete rule INPUT handle")
                .count(),
            2,
            "{:?}",
            k.commits
        );
        assert_eq!(
            k.ipv6.rules,
            vec![("INPUT", k.ipv6.rules[0].1, Rule::Other)]
        );
        assert!(k.ipv4.rules.is_empty(), "{:?}", k.ipv4.rules);
    }

    #[tokio::test]
    async fn a_rule_gone_since_the_read_fails_the_batch_whole() {
        let kernel = armed_model();
        let mut channel = crate::nftables::model::Channel::new(kernel.clone());
        let mut removals = Vec::new();
        for family in Family::BOTH {
            let half = Direction::Input.half();
            let layout = crate::nftables::read_half(&mut channel, family, half).unwrap();
            removals.push(Removal {
                family,
                half,
                layout,
            });
        }
        // Something removed the IPv6 hook between the read and the commit.
        kernel
            .lock()
            .unwrap()
            .ipv6
            .rules
            .retain(|(chain, _, _)| *chain != "INPUT");
        let before = kernel.lock().unwrap().ipv4.clone();
        let generation = crate::nftables::generation(&mut channel).unwrap();
        let (batch, _, end) = crate::nftables::removal_batch(&removals, generation).unwrap();
        let error = crate::nftables::commit(&mut channel, &batch, end).unwrap_err();
        assert!(format!("{error:#}").contains("No such file"), "{error:#}");
        let k = kernel.lock().unwrap();
        assert_eq!(
            k.ipv4, before,
            "a failed batch must leave the other family untouched"
        );
        assert!(k.commits.is_empty());
    }

    #[tokio::test]
    async fn a_rule_replaced_since_the_read_is_left_alone() {
        // A rule replaced in place keeps its handle. A batch built from a
        // read taken before the replacement would delete the replacement,
        // someone else's rule, and succeed. The batch is pinned to the
        // generation it read; a moved generation re-reads.
        use crate::nftables::model::Rule;
        let kernel = armed_model();
        let hook = {
            let k = kernel.lock().unwrap();
            k.ipv4
                .rules
                .iter()
                .find(|(chain, _, _)| *chain == "INPUT")
                .unwrap()
                .1
        };
        kernel.lock().unwrap().before_batch = Some(Box::new(move |k| {
            k.replace_rule(Family::Ipv4, hook, Rule::Other);
        }));
        let gate = IptablesGate::with_netfilter(
            RecordingRunner::default(),
            ModelNetfilter::new(kernel.clone()),
        );
        gate.open().await.unwrap();
        let k = kernel.lock().unwrap();
        assert!(
            k.ipv4
                .rules
                .iter()
                .any(|(c, h, r)| *c == "INPUT" && *h == hook && *r == Rule::Other),
            "the replacement rule was deleted: {:?}",
            k.ipv4.rules
        );
        assert!(!holds_gate(&k.ipv6, GATE_INPUT_CHAIN), "{:?}", k.ipv6);
    }

    #[tokio::test]
    async fn an_unverified_gate_that_nftables_does_not_hold_is_confirmed_absent() {
        // A removal with no verification before it (the source's resume)
        // cannot tell a gate nf_tables does not hold from a gate that is not
        // there. iptables is asked: if it shows the gate, the removal fails
        // instead of reporting it open.
        let runner = ScriptedRunner {
            dump: ARMED_V4_DUMP.to_string(),
            calls: Arc::new(Mutex::new(Vec::new())),
        };
        let kernel = Arc::new(Mutex::new(crate::nftables::model::Kernel::default()));
        let gate = IptablesGate::with_netfilter(runner, ModelNetfilter::new(kernel.clone()));
        let error = gate.open().await.unwrap_err();
        assert!(
            format!("{error:#}").contains("nf_tables backend"),
            "{error:#}"
        );
    }

    #[tokio::test]
    async fn removal_of_an_absent_gate_commits_nothing() {
        let kernel = Arc::new(Mutex::new(crate::nftables::model::Kernel::default()));
        let gate = IptablesGate::with_netfilter(
            RecordingRunner::default(),
            ModelNetfilter::new(kernel.clone()),
        );
        gate.open().await.unwrap();
        assert!(kernel.lock().unwrap().commits.is_empty());
    }

    #[tokio::test]
    async fn a_gate_iptables_reports_but_nftables_does_not_hold_fails_closed() {
        // iptables (by `-S`) says the gate is armed, nf_tables holds nothing:
        // the gate lives where this removal cannot reach (an iptables-legacy
        // binary would do this). Reporting it open would publish a clone that
        // still rejects every new connection.
        let runner = ScriptedRunner {
            dump: ARMED_V4_DUMP.to_string(),
            calls: Arc::new(Mutex::new(Vec::new())),
        };
        let kernel = Arc::new(Mutex::new(crate::nftables::model::Kernel::default()));
        let gate = IptablesGate::with_netfilter(runner, ModelNetfilter::new(kernel.clone()));
        assert!(gate
            .family_is_closed("iptables", IPV4_LOOPBACK)
            .await
            .unwrap());
        let error = gate.open_egress().await.unwrap_err();
        assert!(
            format!("{error:#}").contains("nf_tables backend"),
            "{error:#}"
        );
        assert!(kernel.lock().unwrap().commits.is_empty());
    }

    #[tokio::test]
    async fn snapshot_boundary_closes_new_flows_before_cookie_dump() {
        let state = Arc::new(Mutex::new(ModelState {
            live: vec![socket(11, [198, 51, 100, 8])],
            ..Default::default()
        }));
        let gate = ModelGate(state.clone());
        let barrier = ModelBarrier(state.clone());
        state.lock().unwrap().live_routes = model_routes();
        let mut diag = ModelDiag(state.clone());
        let store = MemoryStore::default();

        assert_eq!(
            prepare_with(&gate, &barrier, &mut diag, &store)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            state.lock().unwrap().events,
            vec!["gate-close", "rx-barrier", "cookie-dump"]
        );
        let state_guard = state.lock().unwrap();
        assert!(state_guard.gate_closed);
        assert!(!state_guard.link_down, "the source's link was taken down");
        drop(state_guard);
        assert_eq!(store.load().unwrap().sockets[0].id.cookie, [11, 0]);
    }

    /// The boundary must hand back the route table it took away.
    ///
    /// The boundary must not purge the route table at all.
    ///
    /// This test used to prove the opposite: that the link-down emptied the
    /// table and the boundary put it back. That repair was the design, and it
    /// was incomplete twice - routes were reinstated (ee3f5c30) but neighbours
    /// were not, and the missing neighbour entry made published ports die about
    /// 1 snapshot in 10. The link-down is gone, so nothing is purged and there
    /// is nothing to reinstate. This is what keeps it that way.
    #[tokio::test]
    async fn the_boundary_never_purges_the_route_table() {
        let state = Arc::new(Mutex::new(ModelState::default()));
        let gate = ModelGate(state.clone());
        let barrier = ModelBarrier(state.clone());
        state.lock().unwrap().live_routes = model_routes();
        let mut diag = ModelDiag(state.clone());
        let store = MemoryStore::default();

        prepare_with(&gate, &barrier, &mut diag, &store)
            .await
            .expect("preparation");
        assert_eq!(
            state.lock().unwrap().live_routes,
            model_routes(),
            "preparing the boundary purged the source's routes"
        );

        restore_with(
            &gate,
            &barrier,
            &mut diag,
            &store,
            &NoRestoreHook,
            CLEANUP_BUDGET,
        )
        .await
        .expect("restore");
        assert_eq!(
            state.lock().unwrap().live_routes,
            model_routes(),
            "restoring purged the clone's routes"
        );
        assert!(
            store.load().is_err(),
            "the manifest must be retired after cleanup"
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
        let barrier = ModelBarrier(state.clone());
        state.lock().unwrap().live_routes = model_routes();
        let mut diag = ModelDiag(state);
        let store = MemoryStore::default();

        assert_eq!(
            prepare_with(&gate, &barrier, &mut diag, &store)
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
        let barrier = ModelBarrier(state.clone());
        state.lock().unwrap().live_routes = model_routes();
        let mut diag = ModelDiag(state.clone());
        let store = MemoryStore::default();

        prepare_with(&gate, &barrier, &mut diag, &store)
            .await
            .expect_err("snapshot preparation must fail when cookie capture fails");
        let state = state.lock().unwrap();
        assert_eq!(
            state.events,
            vec!["gate-close", "rx-barrier", "cookie-dump", "gate-open",]
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
        let barrier = ModelBarrier(state.clone());
        state.lock().unwrap().live_routes = model_routes();
        let mut diag = ModelDiag(state.clone());
        let store = MemoryStore::default();

        prepare_with(&gate, &barrier, &mut diag, &store)
            .await
            .expect_err("snapshot preparation must fail without a receive grace period");
        let state = state.lock().unwrap();
        assert_eq!(state.events, vec!["gate-close", "rx-barrier", "gate-open",]);
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
        let barrier = ModelBarrier(state.clone());
        state.lock().unwrap().live_routes = model_routes();
        let mut diag = ModelDiag(state.clone());
        let store = MemoryStore::default();

        prepare_with(&gate, &barrier, &mut diag, &store)
            .await
            .expect_err("partial gate installation must abort snapshot preparation");
        let state = state.lock().unwrap();
        assert_eq!(state.events, vec!["gate-close", "gate-open",]);
        assert!(!state.gate_closed);
        assert!(!state.link_down);
    }

    #[tokio::test]
    async fn cookie_bound_restore_never_kills_tuple_aba_replacement() {
        let stale = socket(21, [198, 51, 100, 8]);
        let replacement = socket(22, [198, 51, 100, 8]);
        let state = Arc::new(Mutex::new(ModelState {
            gate_closed: true,
            egress_closed: true,
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
        let barrier = ModelBarrier(state.clone());
        state.lock().unwrap().live_routes = model_routes();
        let mut diag = ModelDiag(state.clone());

        // The manifest names the stale socket, which the kernel has already
        // retired, and the live table holds only its tuple-identical
        // replacement. A cookie-bound destroy must therefore report
        // already_gone and leave the replacement alone; a tuple-based one would
        // report a destroy and take the wrong socket with it.
        let report = restore_with(
            &gate,
            &barrier,
            &mut diag,
            &store,
            &NoRestoreHook,
            CLEANUP_BUDGET,
        )
        .await
        .unwrap();
        assert_eq!(
            report.tally,
            CleanupTally {
                destroyed: 0,
                already_gone: 1,
                datagrams: 1,
            }
        );
        assert!(report.verified_armed);
        let state = state.lock().unwrap();
        assert_eq!(state.live, vec![replacement]);
        assert_eq!(
            state.events,
            vec![
                "gate-verify",
                "gate-udp-ingress",
                "rx-barrier",
                "gate-open-egress",
                "cookie-destroy",
                "gate-open",
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
            egress_closed: true,
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
        let barrier = ModelBarrier(state.clone());
        let mut diag = ModelDiag(state.clone());

        let report = restore_with(
            &gate,
            &barrier,
            &mut diag,
            &store,
            &NoRestoreHook,
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
                "gate-udp-ingress",
                "rx-barrier",
                "gate-open-egress",
                "cookie-destroy",
                "gate-open",
            ]
        );
    }

    /// A raised link is exactly what a privileged workload can produce, and
    /// packets can arrive on it: the boundary must down it and take a fresh
    /// grace period before retiring any socket, whatever the gate says.
    #[tokio::test]
    async fn restore_re_barriers_before_cleanup_without_touching_the_link() {
        let stale = socket(29, [198, 51, 100, 8]);
        let state = Arc::new(Mutex::new(ModelState {
            gate_closed: true,
            egress_closed: true,
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
        let barrier = ModelBarrier(state.clone());
        let mut diag = ModelDiag(state.clone());

        let report = restore_with(
            &gate,
            &barrier,
            &mut diag,
            &store,
            &NoRestoreHook,
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
        assert!(
            !events.contains(&"link-down"),
            "restore took the link down: the boundary must not purge the clone's \
             routes and neighbours"
        );
        let destroy = events
            .iter()
            .position(|e| *e == "cookie-destroy")
            .expect("cleanup");
        assert!(
            barrier < destroy,
            "the receive barrier must run BEFORE cleanup: {events:?}"
        );
    }

    /// The gate chains frozen into a current snapshot are verified, not
    /// reinstalled. The receive barrier still runs regardless, because a
    /// privileged workload sharing this namespace can put packets in flight
    /// between any observation and the cleanup.
    #[tokio::test]
    async fn restore_verifies_the_armed_boundary_instead_of_reasserting_it() {
        let stale = socket(26, [198, 51, 100, 8]);
        let state = Arc::new(Mutex::new(ModelState {
            gate_closed: true,
            egress_closed: true,
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
        let barrier = ModelBarrier(state.clone());
        let mut diag = ModelDiag(state.clone());

        let report = restore_with(
            &gate,
            &barrier,
            &mut diag,
            &store,
            &NoRestoreHook,
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
                "gate-udp-ingress",
                "rx-barrier",
                "gate-open-egress",
                "cookie-destroy",
                "gate-open",
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
        let barrier = ModelBarrier(state.clone());
        let mut diag = ModelDiag(state.clone());

        let report = restore_with(
            &gate,
            &barrier,
            &mut diag,
            &store,
            &NoRestoreHook,
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
                "gate-udp-ingress",
                "rx-barrier",
                "gate-open-egress",
                "cookie-destroy",
                "gate-open",
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
            egress_closed: true,
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
        let barrier = ModelBarrier(state.clone());
        state.lock().unwrap().live_routes = model_routes();
        let mut diag = ModelDiag(state.clone());

        restore_with(
            &gate,
            &barrier,
            &mut diag,
            &store,
            &NoRestoreHook,
            CLEANUP_BUDGET,
        )
        .await
        .expect_err("a failed exact destroy must fail restore closed");
        let state = state.lock().unwrap();
        assert_eq!(
            state.events,
            vec![
                "gate-verify",
                "gate-udp-ingress",
                "rx-barrier",
                "gate-open-egress",
                "cookie-destroy"
            ]
        );
        assert!(state.gate_closed, "failed restore published ingress");
        assert!(
            !state.link_down,
            "a failed restore must not have downed the link"
        );
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
            egress_closed: true,
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
        let barrier = ModelBarrier(state.clone());
        state.lock().unwrap().live_routes = model_routes();
        let mut diag = ModelDiag(state.clone());

        let error = restore_with(
            &gate,
            &barrier,
            &mut diag,
            &store,
            &NoRestoreHook,
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
            vec![
                "gate-verify",
                "gate-udp-ingress",
                "rx-barrier",
                "gate-open-egress",
                "cookie-destroy"
            ]
        );
        assert!(state.gate_closed);
        assert!(!state.link_down, "the boundary took the link down");
    }

    /// A hook that outlives its budget is killed, not left running: the client the
    /// guest spawned is gone once the budget's error is returned.
    ///
    /// RED BEFORE THE FIX: the timed-out future was dropped and the process ran on.
    #[tokio::test]
    async fn a_hook_past_its_budget_is_killed() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let argv: Vec<String> = [
            "sh",
            "-c",
            &format!("echo $$ > {}; sleep 30", pid_file.display()),
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        let error = run_bounded(&argv, std::time::Duration::from_millis(300))
            .await
            .expect_err("a 30 s command cannot finish in 300 ms");
        assert!(format!("{error:#}").contains("did not finish"), "{error:#}");
        let pid: u32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // The shell is reaped by the kill; a dead-but-unreaped child would still
        // have a /proc entry, so this reads as "gone" only once it is killed and
        // waited for.
        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "the hook's shell (pid {pid}) is still running after the budget"
        );
    }

    /// At most 64 KiB of each stream is kept; the rest is drained so the hook is
    /// never blocked on a full pipe.
    #[tokio::test]
    async fn a_hooks_output_is_bounded() {
        let argv: Vec<String> = [
            "sh",
            "-c",
            "head -c 1000000 /dev/zero | tr '\\0' x; echo err >&2",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        let output = run_bounded(&argv, std::time::Duration::from_secs(30))
            .await
            .expect("a bounded run of a chatty command");
        assert!(output.status.success());
        assert_eq!(output.stdout.len() as u64, RESTORE_HOOK_OUTPUT_LIMIT);
        assert_eq!(String::from_utf8_lossy(&output.stderr).trim(), "err");
    }

    /// The restore hook is decided and run at publication, after the NFS remount
    /// and the exec and egress-proxy waits. The first version ran it inside the
    /// socket cleanup, before `handle_clone_restore` remounted the shares, so a
    /// hook that read an `--nfs` share blocked on the dead hard mount until its
    /// timeout and failed the restore (a Codex finding on PR 1112). That sequence
    /// is a run of system calls with no seam a runtime test can hold, so this
    /// pins the source order of `handle_clone_restore` itself.
    ///
    /// RED BEFORE THE FIX: `hook_for_this_restore(` came before `mount_nfs_shares(`.
    #[test]
    fn the_restore_hook_runs_after_the_nfs_remount_and_the_proxy_waits() {
        let body = crate::restore::handle_clone_restore_source();
        let at = |needle: &str| {
            body.find(needle)
                .unwrap_or_else(|| panic!("{needle} is not in handle_clone_restore"))
        };
        let nfs = at("mount_nfs_shares(");
        let waits = at("tokio::try_join!(exec_wait, egress_wait)");
        let publish = at(".publish(restore_hook)");
        assert!(
            nfs < publish,
            "the hook runs before the NFS shares are remounted: a hook reading a share \
             blocks on the dead hard mount"
        );
        assert!(
            waits < publish,
            "the hook runs before the egress proxy and exec server are back"
        );
    }

    /// The inherited neighbours are flushed before the clone takes its new IPv6
    /// and announces it. The announcement brings in the traffic that was
    /// waiting for the address (a client that dialled during the restore),
    /// and the guest resolves the client's address to answer it; a flush
    /// running then (up to ten dump-and-delete rounds) deletes those entries
    /// mid-resolution and left one INCOMPLETE until its retransmit a second
    /// later. Caught under packet capture: 2 of 1700 routed restores with an
    /// early client took 946 and 977 ms for the first connection after the
    /// ACK, each with a storm of guest solicitations during the flush and the
    /// next one at +976 ms. Source-pinned like the test above: the window is a
    /// kernel interleaving no runtime test can hold open.
    #[test]
    fn inherited_neighbours_are_flushed_before_the_new_ipv6_is_announced() {
        let body = crate::restore::handle_clone_restore_source();
        let at = |needle: &str| {
            body.find(needle)
                .unwrap_or_else(|| panic!("{needle} is not in handle_clone_restore"))
        };
        assert!(
            at("network::flush_stale_neighbours().await") < at("network::reconfigure_ipv6("),
            "the neighbour flush runs after the new IPv6 is announced, while the traffic \
             the announcement admits is resolving neighbours"
        );
    }

    /// A routed clone's IPv6 gateway is pinned after the neighbour flush and
    /// before the IPv6 swap (#1160). A resolution of the gateway in flight
    /// across the swap is sourced from the snapshot-time address the swap
    /// deletes; the bridge's answer to it was then not delivered and every
    /// reply waited a second for the retransmit. A pin before the flush would
    /// survive it only because the flush skips permanent entries, which is
    /// the wrong thing to depend on; a pin after the swap leaves the window
    /// open.
    #[test]
    fn the_routed_gateway_is_pinned_before_the_ipv6_swap() {
        let body = crate::restore::handle_clone_restore_source();
        let at = |needle: &str| {
            body.find(needle)
                .unwrap_or_else(|| panic!("{needle} is not in handle_clone_restore"))
        };
        let flush = at("network::flush_stale_neighbours().await");
        let pin = at("network::pin_routed_gateway_neighbour().await");
        let swap = at("network::reconfigure_ipv6(");
        assert!(
            flush < pin && pin < swap,
            "flush={flush} pin={pin} swap={swap}"
        );
    }

    /// A failed NFS remount fails the restore. The remount runs before the hook
    /// so that a hook can use the share; a remount that failed and was only
    /// logged left the mount point an ordinary directory, so a hook writing
    /// there would succeed against the wrong place and the clone would publish
    /// with its share missing (a Codex finding on PR 1112). Like the ordering
    /// test above, this pins the source: the remount is a system call sequence
    /// with no seam a runtime test can hold.
    ///
    /// RED BEFORE THE FIX: the failure was an eprintln WARNING and the restore
    /// went on to publish.
    #[test]
    fn a_failed_nfs_remount_keeps_the_clone_unpublished() {
        let body = crate::restore::handle_clone_restore_source();
        let call = body
            .find("mount_nfs_shares(")
            .expect("the restore remounts the NFS shares");
        let statement = &body[call..call + body[call..].find(';').expect("statement end") + 1];
        assert!(
            !body.contains("NFS remount after restore failed"),
            "the NFS remount failure is still only logged"
        );
        assert!(
            statement.contains(".context(") && statement.trim_end().ends_with("?;"),
            "the NFS remount failure is not propagated: {statement}"
        );
    }

    /// Whether the hook has a container to run in is the container's state at
    /// publication, asked of podman, not a marker set when the agent was about to
    /// start it. A user snapshot taken between that marker and podman's start
    /// recorded a container that did not exist, and its clones failed their
    /// restore on `podman exec`; the opposite order would have skipped the hook
    /// for a container that did exist.
    ///
    /// RED BEFORE THE FIX: `CONTAINER_STARTED` was stored in agent.rs and read in
    /// restore.rs.
    #[test]
    fn the_hook_asks_the_container_state_not_a_marker() {
        for (name, source) in [
            ("restore.rs", include_str!("restore.rs")),
            ("agent.rs", include_str!("agent.rs")),
        ] {
            assert!(
                !source.contains("CONTAINER_STARTED") && !source.contains("mark_container_started"),
                "{name} still carries the container-started marker"
            );
        }
        let argv = ContainerRestoreHook::inspect_argv();
        assert!(
            argv.windows(3)
                .any(|w| w == ["inspect", "--format", "{{.State.Running}}"]),
            "the hook does not read the container's state: {argv:?}"
        );
    }

    /// The decision from podman's answer: a running container runs the hook; a
    /// stopped one, or none at all, skips it with the reason; podman failing for
    /// any other reason fails the restore closed, because a clone published
    /// without its stale-state repair is the failure the hook exists to prevent.
    ///
    /// RED BEFORE THE FIX: every nonzero exit skipped the hook.
    #[test]
    fn the_hook_runs_only_in_a_running_container() {
        use std::os::unix::process::ExitStatusExt;
        let output = |code: i32, stdout: &str, stderr: &str| BoundedOutput {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        };
        assert_eq!(
            hook_eligibility(&output(0, "true\n", "")),
            HookEligibility::Run
        );
        assert_eq!(
            hook_eligibility(&output(0, "false\n", "")),
            HookEligibility::Skip("fcvm-container is not running (Running=false)".to_string())
        );
        assert_eq!(
            hook_eligibility(&output(
                125,
                "",
                "Error: no container with name or ID \"fcvm-container\" found"
            )),
            HookEligibility::Skip(
                "podman inspect exited exit status: 125: Error: no container with name or ID \
                 \"fcvm-container\" found"
                    .to_string()
            )
        );
        assert_eq!(
            hook_eligibility(&output(
                125,
                "",
                "Error: unable to connect to Podman socket: dial unix /run/podman/podman.sock: connect: no such file or directory"
            )),
            HookEligibility::Fail(
                "podman inspect exited exit status: 125: Error: unable to connect to Podman \
                 socket: dial unix /run/podman/podman.sock: connect: no such file or directory"
                    .to_string()
            )
        );
    }

    /// The hook runs with the egress half of the gate open and the ingress half
    /// closed: it can reach out, and nothing from outside reaches the clone yet.
    /// The model hook asserts both; the event order shows the egress half opened
    /// after the barrier, before the first socket is retired (a daemon whose
    /// socket goes first reconnects at once), and before the hook.
    ///
    /// RED BEFORE THE FIX: the hook ran with both halves closed, so its own and
    /// the workload's new connections were answered with resets; then egress
    /// opened only after the last retirement, and the first daemon's reconnect
    /// still met a reset.
    #[tokio::test]
    async fn the_restore_hook_runs_with_egress_open_and_ingress_gated() {
        let stale = socket(41, [198, 51, 100, 41]);
        let state = Arc::new(Mutex::new(ModelState {
            gate_closed: true,
            egress_closed: true,
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
        let barrier = ModelBarrier(state.clone());
        state.lock().unwrap().live_routes = model_routes();
        let mut diag = ModelDiag(state.clone());
        let hook = ModelHook(state.clone());

        let report = restore_with(&gate, &barrier, &mut diag, &store, &hook, CLEANUP_BUDGET)
            .await
            .expect("restore with a hook");
        let state = state.lock().unwrap();
        assert_eq!(
            state.events,
            vec![
                "gate-verify",
                "gate-udp-ingress",
                "rx-barrier",
                "gate-open-egress",
                "cookie-destroy",
                "restore-hook",
                "gate-open"
            ],
            "the UDP rejection joins the verified gate, egress opens after the barrier and before the retirement, ingress after the hook"
        );
        assert!(!state.gate_closed && !state.egress_closed);
        assert!(report.egress_open_ms >= 0.0);
    }

    /// The command runs in the container under its own `timeout`, so a hook that
    /// hangs ends inside the container too, not only at the podman client.
    #[test]
    fn the_hook_runs_under_the_containers_own_timeout() {
        let hook = ContainerRestoreHook {
            command: "curl -sf http://[::1]:8088/x".to_string(),
        };
        let argv = hook.argv();
        let container = argv
            .iter()
            .position(|a| a == "fcvm-container")
            .expect("the container");
        assert_eq!(
            &argv[container + 1..],
            &[
                "timeout",
                "-k",
                &RESTORE_HOOK_KILL_AFTER_SECS.to_string(),
                &RESTORE_HOOK_COMMAND_SECS.to_string(),
                "sh",
                "-c",
                "curl -sf http://[::1]:8088/x",
            ]
        );
        assert!(
            RESTORE_HOOK_BUDGET.as_secs()
                > RESTORE_HOOK_COMMAND_SECS + RESTORE_HOOK_KILL_AFTER_SECS,
            "the client budget must outlast the container's timeout and its kill grace"
        );
    }

    /// The restore hook runs inside the boundary: after the egress half of the
    /// gate opens and the snapshot-time sockets are retired, and before the
    /// ingress half opens. A process that must redo work a snapshot made stale
    /// (service-discovery data as old as the snapshot) gets to do it, with the
    /// network, before any published-port client can reach the clone.
    ///
    /// RED BEFORE THE FIX: the boundary ran no hook at all.
    #[tokio::test]
    async fn the_restore_hook_runs_after_cleanup_and_before_the_clone_is_published() {
        let stale = socket(31, [198, 51, 100, 9]);
        let state = Arc::new(Mutex::new(ModelState {
            gate_closed: true,
            egress_closed: true,
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
        let barrier = ModelBarrier(state.clone());
        state.lock().unwrap().live_routes = model_routes();
        let mut diag = ModelDiag(state.clone());
        let hook = ModelHook(state.clone());

        restore_with(&gate, &barrier, &mut diag, &store, &hook, CLEANUP_BUDGET)
            .await
            .expect("restore with a hook");
        let state = state.lock().unwrap();
        assert_eq!(
            state.events,
            vec![
                "gate-verify",
                "gate-udp-ingress",
                "rx-barrier",
                "gate-open-egress",
                "cookie-destroy",
                "restore-hook",
                "gate-open"
            ],
            "the hook runs after the sockets are retired, with egress open, before ingress opens"
        );
        assert!(!state.gate_closed);
    }

    /// A hook that fails leaves the clone unpublished: the ingress half of the gate
    /// stays closed and the restore reports the failure, the same way a failed
    /// socket retirement does.
    ///
    /// RED BEFORE THE FIX: the boundary ran no hook at all.
    #[tokio::test]
    async fn a_failed_restore_hook_keeps_the_restored_clone_closed() {
        let stale = socket(32, [198, 51, 100, 10]);
        let state = Arc::new(Mutex::new(ModelState {
            gate_closed: true,
            egress_closed: true,
            live: vec![stale],
            fail_hook: true,
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
        let barrier = ModelBarrier(state.clone());
        state.lock().unwrap().live_routes = model_routes();
        let mut diag = ModelDiag(state.clone());
        let hook = ModelHook(state.clone());

        let error = restore_with(&gate, &barrier, &mut diag, &store, &hook, CLEANUP_BUDGET)
            .await
            .expect_err("a failed restore hook must prevent clone publication");
        assert!(
            format!("{error:#}").contains("restore hook"),
            "unexpected diagnostic: {error:#}"
        );
        let state = state.lock().unwrap();
        assert_eq!(
            state.events,
            vec![
                "gate-verify",
                "gate-udp-ingress",
                "rx-barrier",
                "gate-open-egress",
                "cookie-destroy",
                "restore-hook"
            ]
        );
        assert!(state.gate_closed, "the gate opened after the hook failed");
    }

    #[tokio::test]
    async fn exhausted_cleanup_budget_keeps_the_restored_clone_closed() {
        let stale = socket(25, [198, 51, 100, 8]);
        let state = Arc::new(Mutex::new(ModelState {
            gate_closed: true,
            egress_closed: true,
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
        let barrier = ModelBarrier(state.clone());
        state.lock().unwrap().live_routes = model_routes();
        let mut diag = ModelDiag(state.clone());

        // Each destroy carries a five-second receive timeout, so an unbounded
        // loop over a large manifest can hold the clone indefinitely. With no
        // budget left the cleanup must fail closed before touching a socket,
        // never publish, and never retire the manifest.
        let error = restore_with(
            &gate,
            &barrier,
            &mut diag,
            &store,
            &NoRestoreHook,
            std::time::Duration::ZERO,
        )
        .await
        .expect_err("an exhausted cleanup budget must fail restore closed");
        assert!(
            format!("{error:#}").contains("exceeded its"),
            "unexpected diagnostic: {error:#}"
        );

        let state = state.lock().unwrap();
        assert_eq!(
            state.events,
            vec![
                "gate-verify",
                "gate-udp-ingress",
                "rx-barrier",
                "gate-open-egress"
            ]
        );
        assert!(state.gate_closed, "a timed-out cleanup published ingress");
        assert!(
            !state.link_down,
            "a timed-out cleanup took the link down: even the failure path must \
             not purge the clone's routes and neighbours"
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
        let displayed = socket(31, [198, 51, 100, 8]);
        let kernel =
            SharedKernel::new(&[displayed], ModelAnswer::Displayed, ModelDelivery::InOrder);
        let error = CookieDestroyer::new(kernel.opener())
            .destroy(&[displayed])
            .expect_err("a displayed tuple without NLMSG_ERROR ACK must fail closed");
        assert!(
            format!("{error:#}").contains("not a destruction oracle"),
            "unexpected diagnostic: {error:#}"
        );
    }

    #[test]
    fn netlink_ack_is_the_only_positive_destroy_oracle() {
        let live = socket(91, [198, 51, 100, 8]);
        let kernel = SharedKernel::new(&[live], ModelAnswer::Kernel, ModelDelivery::InOrder);
        assert_eq!(
            CookieDestroyer::new(kernel.opener())
                .destroy(&[live])
                .unwrap(),
            vec![DestroyOutcome::Destroyed]
        );
    }

    /// The restore cost that mattered was per cookie: a socket opened, bound
    /// and connected, and a blocking round trip, for every snapshot-time
    /// socket. A large manifest must cost one socket and one datagram per
    /// batch, and never queue more acknowledgements than a batch.
    #[tokio::test]
    async fn a_large_manifest_is_retired_over_one_socket_in_bounded_batches() {
        let sockets: Vec<TcpSocketIdentity> = (0..1000)
            .map(|cookie| socket(1000 + cookie, [198, 51, 100, 8]))
            .collect();
        let kernel = SharedKernel::new(&sockets, ModelAnswer::Kernel, ModelDelivery::InOrder);
        let state = Arc::new(Mutex::new(ModelState {
            gate_closed: true,
            egress_closed: true,
            ..Default::default()
        }));
        let store = MemoryStore::default();
        store
            .save(&SnapshotNetworkManifest {
                version: MANIFEST_VERSION,
                routes: Vec::new(),
                sockets: sockets.clone(),
            })
            .unwrap();
        let mut diag = ChannelDiag(CookieDestroyer::new(kernel.opener()));

        let report = restore_with(
            &ModelGate(state.clone()),
            &ModelBarrier(state.clone()),
            &mut diag,
            &store,
            &NoRestoreHook,
            CLEANUP_BUDGET,
        )
        .await
        .unwrap();

        assert_eq!(
            report.tally,
            CleanupTally {
                destroyed: 1000,
                already_gone: 0,
                datagrams: 16,
            }
        );
        let kernel = kernel.0.lock().unwrap();
        assert!(
            kernel.live.is_empty(),
            "{} sockets survived",
            kernel.live.len()
        );
        assert_eq!(kernel.opens, 1, "the cleanup opened more than one socket");
        assert_eq!(
            kernel.datagrams.len(),
            1000usize.div_ceil(DESTROY_BATCH),
            "requests per datagram: {:?}",
            kernel.datagrams
        );
        assert!(
            kernel.most_queued <= DESTROY_BATCH,
            "{} acknowledgements were queued at once",
            kernel.most_queued
        );
        assert_eq!(kernel.receives, 1000, "one receive per acknowledgement");
        assert!(
            !state.lock().unwrap().gate_closed,
            "cleanup never published"
        );
    }

    /// A guest, or a privileged workload in it, can lower
    /// net.core.rmem_default below what a batch of acknowledgements needs. The
    /// kernel drops each acknowledgement that does not fit and the next receive
    /// fails with ENOBUFS, which would fail every restore with a large manifest
    /// closed. The destroyer has to make room first, and fit its batches to the
    /// buffer it got when it cannot.
    #[test]
    fn a_lowered_receive_buffer_drops_no_acknowledgement() {
        let sockets: Vec<TcpSocketIdentity> = (0..200)
            .map(|cookie| socket(4000 + cookie, [198, 51, 100, 8]))
            .collect();
        // Room for 16 acknowledgements, as with rmem_default at 13,312 bytes.
        let lowered = 16 * MODEL_ACK_CHARGE;
        for can_force in [true, false] {
            let kernel = SharedKernel::new(&sockets, ModelAnswer::Kernel, ModelDelivery::InOrder);
            {
                let mut model = kernel.0.lock().unwrap();
                model.receive_capacity = lowered;
                model.can_force = can_force;
            }
            let outcomes = CookieDestroyer::new(kernel.opener())
                .destroy(&sockets)
                .unwrap_or_else(|error| panic!("can_force={can_force}: {error:#}"));
            assert!(outcomes
                .iter()
                .all(|outcome| *outcome == DestroyOutcome::Destroyed));
            let model = kernel.0.lock().unwrap();
            assert_eq!(
                model.dropped, 0,
                "can_force={can_force}: acknowledgements dropped"
            );
            assert!(
                model.live.is_empty(),
                "can_force={can_force}: sockets survived"
            );
            if can_force {
                assert_eq!(
                    model.datagrams[0], DESTROY_BATCH,
                    "a forced buffer holds a batch"
                );
            } else {
                assert!(
                    model.datagrams[0] * MODEL_ACK_CHARGE <= lowered,
                    "a batch of {} does not fit {lowered} bytes",
                    model.datagrams[0]
                );
            }
        }
    }

    /// The cleanup's counts reach the record fcvm prints as guest_phases.
    /// Without them the time a restore spent destroying sockets cannot be
    /// divided by how many it destroyed.
    #[tokio::test]
    async fn the_restore_record_carries_the_cleanup_counts() {
        let sockets: Vec<TcpSocketIdentity> = (0..150)
            .map(|cookie| socket(6000 + cookie, [198, 51, 100, 8]))
            .collect();
        // 100 are live and 50 already gone.
        let kernel =
            SharedKernel::new(&sockets[..100], ModelAnswer::Kernel, ModelDelivery::InOrder);
        let state = Arc::new(Mutex::new(ModelState {
            gate_closed: true,
            egress_closed: true,
            ..Default::default()
        }));
        let store = MemoryStore::default();
        store
            .save(&SnapshotNetworkManifest {
                version: MANIFEST_VERSION,
                routes: Vec::new(),
                sockets: sockets.clone(),
            })
            .unwrap();
        let mut diag = ChannelDiag(CookieDestroyer::new(kernel.opener()));
        let report = restore_with(
            &ModelGate(state.clone()),
            &ModelBarrier(state.clone()),
            &mut diag,
            &store,
            &NoRestoreHook,
            CLEANUP_BUDGET,
        )
        .await
        .unwrap();

        let mut phases = crate::restore::RestorePhases::default();
        phases.record_boundary(&report);
        let record: serde_json::Value = serde_json::from_str(&phases.to_frame_json()).unwrap();
        let datagrams = kernel.0.lock().unwrap().datagrams.len();
        assert_eq!(datagrams, 3, "150 sockets at 64 to a datagram");
        assert_eq!(record["tcp_destroyed"], 100, "{record}");
        assert_eq!(record["tcp_already_gone"], 50, "{record}");
        assert_eq!(record["tcp_destroy_datagrams"], datagrams, "{record}");
    }

    #[test]
    fn acknowledgements_are_matched_to_their_socket_by_sequence_number() {
        let sockets: Vec<TcpSocketIdentity> = (0..5)
            .map(|cookie| socket(60 + cookie, [198, 51, 100, 8]))
            .collect();
        // Only the first two are live. Replies read in arrival order instead
        // of by sequence number would give the reversed and packed deliveries
        // different outcomes.
        let live = [sockets[0], sockets[1]];
        let expected = vec![
            DestroyOutcome::Destroyed,
            DestroyOutcome::Destroyed,
            DestroyOutcome::AlreadyGone,
            DestroyOutcome::AlreadyGone,
            DestroyOutcome::AlreadyGone,
        ];
        for delivery in [
            ModelDelivery::InOrder,
            ModelDelivery::Reversed,
            ModelDelivery::Packed,
        ] {
            let kernel = SharedKernel::new(&live, ModelAnswer::Kernel, delivery);
            let outcomes = CookieDestroyer::new(kernel.opener())
                .destroy(&sockets)
                .unwrap();
            assert_eq!(outcomes, expected, "delivery {delivery:?}");
        }
    }

    #[test]
    fn any_reply_but_a_destroy_acknowledgement_fails_the_batch_closed() {
        let sockets = [
            socket(0x46, [198, 51, 100, 8]),
            socket(0x47, [198, 51, 100, 9]),
        ];
        for (answer, expected) in [
            (ModelAnswer::Errno(libc::EPERM), "failed with errno 1"),
            (
                ModelAnswer::Errno(libc::EOPNOTSUPP),
                "SOCK_DESTROY is unsupported",
            ),
            (ModelAnswer::WrongRequest, "described the wrong request"),
            (ModelAnswer::Silence, "2 of 2 undecided"),
        ] {
            let kernel = SharedKernel::new(&sockets, answer, ModelDelivery::InOrder);
            let error = CookieDestroyer::new(kernel.opener())
                .destroy(&sockets)
                .expect_err("the batch must fail closed");
            let diagnostic = format!("{error:#}");
            assert!(
                diagnostic.contains(expected),
                "{answer:?}: unexpected diagnostic: {diagnostic}"
            );
            if !matches!(answer, ModelAnswer::Silence) {
                assert!(
                    diagnostic.contains("cookie=00000046:00000000"),
                    "{answer:?}: the diagnostic must name the socket: {diagnostic}"
                );
            }
        }
    }
}
