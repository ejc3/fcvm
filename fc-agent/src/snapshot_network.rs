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
use std::os::fd::{FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

const MANIFEST_VERSION: u32 = 1;
const MANIFEST_PATH: &str = "/run/fcvm/snapshot-network.json";
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
}

trait ExternalLink {
    async fn down(&self) -> Result<()>;
    async fn up(&self) -> Result<()>;
}

trait PacketReceiveBarrier {
    fn synchronize(&self) -> Result<()>;
}

trait CommandRunner {
    async fn output(&self, program: &str, args: &[&str]) -> io::Result<std::process::Output>;
}

struct SystemCommandRunner;

impl CommandRunner for SystemCommandRunner {
    async fn output(&self, program: &str, args: &[&str]) -> io::Result<std::process::Output> {
        Command::new(program).args(args).output().await
    }
}

struct IpLink<R> {
    runner: R,
}

impl<R> IpLink<R> {
    fn new(runner: R) -> Self {
        Self { runner }
    }
}

impl<R: CommandRunner + Sync> IpLink<R> {
    async fn set(&self, state: &str) -> Result<()> {
        let args = ["link", "set", "dev", "eth0", state];
        let output = self
            .runner
            .output("ip", &args)
            .await
            .context("spawning ip to change snapshot network link state")?;
        if !output.status.success() {
            bail!(
                "snapshot network link command failed: {}",
                command_failure("ip", &args, &output)
            );
        }
        Ok(())
    }
}

impl<R: CommandRunner + Sync> ExternalLink for IpLink<R> {
    async fn down(&self) -> Result<()> {
        self.set("down").await
    }

    async fn up(&self) -> Result<()> {
        self.set("up").await
    }
}

struct SystemPacketReceiveBarrier;

impl PacketReceiveBarrier for SystemPacketReceiveBarrier {
    fn synchronize(&self) -> Result<()> {
        const ETH_P_ALL: u16 = 0x0003;

        // packet_release() executes synchronize_net(), whose kernel contract is
        // to wait until every packet already in receive processing is done.
        // The NEW-flow gate is installed first and eth0 is down, so packets
        // that begin after this grace period cannot create an uncaptured TCP
        // socket. Merely waiting or taking repeated dumps cannot prove this.
        let raw = unsafe {
            libc::socket(
                libc::AF_PACKET,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC,
                ETH_P_ALL.to_be() as i32,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error()).context(
                "opening AF_PACKET receive-path barrier (guest kernel needs CONFIG_PACKET=y)",
            );
        }
        // SAFETY: `raw` is a newly-created owned fd. Dropping it invokes the
        // packet socket release path and its synchronize_net() grace period.
        drop(unsafe { OwnedFd::from_raw_fd(raw) });
        Ok(())
    }
}

struct IptablesGate<R> {
    runner: R,
}

impl<R> IptablesGate<R> {
    fn new(runner: R) -> Self {
        Self { runner }
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
        self.required(
            program,
            &[
                "-w",
                "-A",
                chain,
                "-p",
                "tcp",
                loopback_flag,
                loopback_network,
                "-j",
                "RETURN",
            ],
        )
        .await?;
        self.required(
            program,
            &[
                "-w",
                "-A",
                chain,
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
            ],
        )
        .await?;
        self.required(program, &["-w", "-A", chain, "-j", "RETURN"])
            .await?;
        Ok(())
    }

    async fn install_hook(&self, program: &str, hook: &str, chain: &str) -> Result<()> {
        let check = ["-w", "-C", hook, "-j", chain];
        let output = self.runner.output(program, &check).await?;
        if !output.status.success() {
            self.required(program, &["-w", "-I", hook, "1", "-j", chain])
                .await?;
        }
        self.required(program, &check).await?;
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
        Ok(())
    }

    async fn remove_chain(&self, program: &str, hook: &str, chain: &str) -> Result<()> {
        if !self.chain_exists(program, chain).await? {
            return Ok(());
        }
        // A hook can legitimately carry the jump more than once (an interrupted
        // close leaves one behind), so removal repeats. It is bounded: an
        // iptables chain that still matches after this many deletions is not
        // converging, and looping forever there would hang restore instead of
        // reporting the stuck rule.
        const MAX_DUPLICATE_HOOKS: usize = 64;
        let check = ["-w", "-C", hook, "-j", chain];
        let mut removed = 0usize;
        while self.runner.output(program, &check).await?.status.success() {
            if removed == MAX_DUPLICATE_HOOKS {
                bail!(
                    "{program} {hook} still jumps to {chain} after removing \
                     {MAX_DUPLICATE_HOOKS} references; refusing to loop"
                );
            }
            self.required(program, &["-w", "-D", hook, "-j", chain])
                .await?;
            removed += 1;
        }
        self.required(program, &["-w", "-F", chain]).await?;
        self.required(program, &["-w", "-X", chain]).await?;
        Ok(())
    }

    async fn remove_family(&self, program: &str) -> Result<()> {
        self.remove_chain(program, "INPUT", GATE_INPUT_CHAIN)
            .await?;
        self.remove_chain(program, "OUTPUT", GATE_OUTPUT_CHAIN)
            .await?;
        Ok(())
    }
}

impl<R: CommandRunner + Sync> FirewallGate for IptablesGate<R> {
    async fn close(&self) -> Result<()> {
        self.install_family("iptables", "127.0.0.0/8")
            .await
            .context("installing IPv4 snapshot network gate")?;
        self.install_family("ip6tables", "::1/128")
            .await
            .context("installing IPv6 snapshot network gate")
    }

    async fn open(&self) -> Result<()> {
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

    // SAFETY: libc socket arguments are constants from the Linux UAPI.
    let raw = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC,
            NETLINK_SOCK_DIAG,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error())
            .context("opening NETLINK_SOCK_DIAG socket (guest kernel needs CONFIG_INET_DIAG=y)");
    }
    // SAFETY: `raw` is a newly-created owned fd.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
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

async fn reopen_with<G: FirewallGate, L: ExternalLink>(gate: &G, link: &L) -> Result<()> {
    // Remove both family-specific hooks while eth0 is still down, then make
    // the link visible as the final publication step.  Removing IPv4 and IPv6
    // hooks cannot be one iptables transaction; doing it behind link-down
    // prevents a partial gate removal from publishing either family.
    gate.open()
        .await
        .context("opening the snapshot TCP NEW-flow gate while eth0 is down")?;
    link.up()
        .await
        .context("bringing eth0 up after opening the snapshot TCP NEW-flow gate")
}

async fn rollback_preparation<G: FirewallGate, L: ExternalLink>(
    error: anyhow::Error,
    gate: &G,
    link: &L,
) -> anyhow::Error {
    match reopen_with(gate, link).await {
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
>(
    gate: &G,
    link: &L,
    receive_barrier: &B,
    diagnostic: &mut D,
    store: &S,
) -> Result<usize> {
    if let Err(error) = gate
        .close()
        .await
        .context("closing snapshot TCP NEW-flow gate")
    {
        return Err(rollback_preparation(error, gate, link).await);
    }
    if let Err(error) = link
        .down()
        .await
        .context("bringing eth0 down before snapshot")
    {
        return Err(rollback_preparation(error, gate, link).await);
    }
    if let Err(error) = receive_barrier
        .synchronize()
        .context("waiting for pre-boundary packet receive processing")
    {
        return Err(rollback_preparation(error, gate, link).await);
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
        Err(error) => return Err(rollback_preparation(error, gate, link).await),
    };
    let manifest = SnapshotNetworkManifest {
        version: MANIFEST_VERSION,
        sockets,
    };
    if let Err(error) = store.save(&manifest) {
        return Err(rollback_preparation(error, gate, link).await);
    }
    Ok(manifest.sockets.len())
}

async fn restore_with<
    G: FirewallGate,
    L: ExternalLink,
    B: PacketReceiveBarrier,
    D: SocketDiagnostic,
    S: ManifestStore,
>(
    gate: &G,
    link: &L,
    receive_barrier: &B,
    diagnostic: &mut D,
    store: &S,
    budget: std::time::Duration,
) -> Result<CleanupTally> {
    // A current snapshot already contains a closed gate and down link.  Repeat
    // both operations idempotently so a malformed/older snapshot cannot be
    // published by the cleanup path before its manifest is rejected.
    gate.close()
        .await
        .context("ensuring restored snapshot TCP NEW-flow gate is closed")?;
    link.down()
        .await
        .context("ensuring restored eth0 remains down during socket cleanup")?;
    receive_barrier
        .synchronize()
        .context("waiting for restored packet receive processing to quiesce")?;
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
    reopen_with(gate, link)
        .await
        .context("publishing restored network after cookie-bound cleanup")?;
    Ok(tally)
}

pub async fn prepare_snapshot_network() -> Result<()> {
    let _transaction_lock = acquire_boundary_lock()?;
    let gate = IptablesGate::new(SystemCommandRunner);
    let link = IpLink::new(SystemCommandRunner);
    let receive_barrier = SystemPacketReceiveBarrier;
    let mut diagnostic = SystemSocketDiagnostic;
    let captured = prepare_with(
        &gate,
        &link,
        &receive_barrier,
        &mut diagnostic,
        &FileManifestStore::default(),
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
    let link = IpLink::new(SystemCommandRunner);
    FileManifestStore::default()
        .remove()
        .context("retiring source snapshot network manifest before publication")?;
    reopen_with(&gate, &link)
        .await
        .context("reopening source VM network")?;
    eprintln!("[fc-agent] source VM snapshot network reopened: link=up gate=open");
    Ok(())
}

pub async fn restore_snapshot_network() -> Result<()> {
    let _transaction_lock = acquire_boundary_lock()?;
    let gate = IptablesGate::new(SystemCommandRunner);
    let link = IpLink::new(SystemCommandRunner);
    let receive_barrier = SystemPacketReceiveBarrier;
    let mut diagnostic = SystemSocketDiagnostic;
    let tally = restore_with(
        &gate,
        &link,
        &receive_barrier,
        &mut diagnostic,
        &FileManifestStore::default(),
        CLEANUP_BUDGET,
    )
    .await?;
    eprintln!(
        "[fc-agent] snapshot network cleanup complete: destroyed={} already_gone={} \
         link=up gate=open",
        tally.destroyed, tally.already_gone
    );
    Ok(())
}

/// Whether this process image was captured behind the snapshot network
/// boundary.  Firecracker normally obtains restore metadata through MMDS on
/// eth0, but a current snapshot deliberately captures eth0 down.  The restore
/// watcher uses this marker to select the host's restore-only vsock control
/// plane before touching MMDS.
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
        events: Vec<&'static str>,
        live: Vec<TcpSocketIdentity>,
        fail_gate_close: bool,
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
    }

    #[derive(Clone)]
    struct ModelLink(Arc<Mutex<ModelState>>);

    impl ExternalLink for ModelLink {
        async fn down(&self) -> Result<()> {
            let mut state = self.0.lock().unwrap();
            state.link_down = true;
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

    struct ModelBarrier(Arc<Mutex<ModelState>>);

    impl PacketReceiveBarrier for ModelBarrier {
        fn synchronize(&self) -> Result<()> {
            let mut state = self.0.lock().unwrap();
            assert!(
                state.gate_closed,
                "receive barrier ran before the NEW-flow gate"
            );
            assert!(state.link_down, "receive barrier ran before eth0 was down");
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
                "restore brought eth0 up before destroying sockets"
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

    #[tokio::test]
    async fn snapshot_boundary_closes_new_flows_before_cookie_dump() {
        let state = Arc::new(Mutex::new(ModelState {
            live: vec![socket(11, [198, 51, 100, 8])],
            ..Default::default()
        }));
        let gate = ModelGate(state.clone());
        let link = ModelLink(state.clone());
        let barrier = ModelBarrier(state.clone());
        let mut diag = ModelDiag(state.clone());
        let store = MemoryStore::default();

        assert_eq!(
            prepare_with(&gate, &link, &barrier, &mut diag, &store)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            state.lock().unwrap().events,
            vec!["gate-close", "link-down", "rx-barrier", "cookie-dump"]
        );
        let state_guard = state.lock().unwrap();
        assert!(state_guard.gate_closed);
        assert!(state_guard.link_down);
        drop(state_guard);
        assert_eq!(store.load().unwrap().sockets[0].id.cookie, [11, 0]);
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
        let mut diag = ModelDiag(state);
        let store = MemoryStore::default();

        assert_eq!(
            prepare_with(&gate, &link, &barrier, &mut diag, &store)
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
        let mut diag = ModelDiag(state.clone());
        let store = MemoryStore::default();

        prepare_with(&gate, &link, &barrier, &mut diag, &store)
            .await
            .expect_err("snapshot preparation must fail when cookie capture fails");
        let state = state.lock().unwrap();
        assert_eq!(
            state.events,
            vec![
                "gate-close",
                "link-down",
                "rx-barrier",
                "cookie-dump",
                "gate-open",
                "link-up"
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
        let mut diag = ModelDiag(state.clone());
        let store = MemoryStore::default();

        prepare_with(&gate, &link, &barrier, &mut diag, &store)
            .await
            .expect_err("snapshot preparation must fail without a receive grace period");
        let state = state.lock().unwrap();
        assert_eq!(
            state.events,
            vec![
                "gate-close",
                "link-down",
                "rx-barrier",
                "gate-open",
                "link-up"
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
        let mut diag = ModelDiag(state.clone());
        let store = MemoryStore::default();

        prepare_with(&gate, &link, &barrier, &mut diag, &store)
            .await
            .expect_err("partial gate installation must abort snapshot preparation");
        let state = state.lock().unwrap();
        assert_eq!(state.events, vec!["gate-close", "gate-open", "link-up"]);
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
                sockets: vec![stale],
            })
            .unwrap();
        let gate = ModelGate(state.clone());
        let link = ModelLink(state.clone());
        let barrier = ModelBarrier(state.clone());
        let mut diag = ModelDiag(state.clone());

        // The manifest names the stale socket, which the kernel has already
        // retired, and the live table holds only its tuple-identical
        // replacement. A cookie-bound destroy must therefore report
        // already_gone and leave the replacement alone; a tuple-based one would
        // report a destroy and take the wrong socket with it.
        assert_eq!(
            restore_with(&gate, &link, &barrier, &mut diag, &store, CLEANUP_BUDGET)
                .await
                .unwrap(),
            CleanupTally {
                destroyed: 0,
                already_gone: 1,
            }
        );
        let state = state.lock().unwrap();
        assert_eq!(state.live, vec![replacement]);
        assert_eq!(
            state.events,
            vec![
                "gate-close",
                "link-down",
                "rx-barrier",
                "cookie-destroy",
                "gate-open",
                "link-up"
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
                sockets: vec![stale],
            })
            .unwrap();
        let gate = ModelGate(state.clone());
        let link = ModelLink(state.clone());
        let barrier = ModelBarrier(state.clone());
        let mut diag = ModelDiag(state.clone());

        restore_with(&gate, &link, &barrier, &mut diag, &store, CLEANUP_BUDGET)
            .await
            .expect_err("a failed exact destroy must fail restore closed");
        let state = state.lock().unwrap();
        assert_eq!(
            state.events,
            vec!["gate-close", "link-down", "rx-barrier", "cookie-destroy"]
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
                sockets: vec![stale],
            })
            .unwrap();
        let gate = ModelGate(state.clone());
        let link = ModelLink(state.clone());
        let barrier = ModelBarrier(state.clone());
        let mut diag = ModelDiag(state.clone());

        let error = restore_with(&gate, &link, &barrier, &mut diag, &store, CLEANUP_BUDGET)
            .await
            .expect_err("manifest retirement failure must prevent clone publication");
        assert!(
            format!("{error:#}").contains("retiring snapshot network manifest"),
            "unexpected diagnostic: {error:#}"
        );
        let state = state.lock().unwrap();
        assert_eq!(
            state.events,
            vec!["gate-close", "link-down", "rx-barrier", "cookie-destroy"]
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
                sockets: vec![stale],
            })
            .unwrap();
        let gate = ModelGate(state.clone());
        let link = ModelLink(state.clone());
        let barrier = ModelBarrier(state.clone());
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
            std::time::Duration::ZERO,
        )
        .await
        .expect_err("an exhausted cleanup budget must fail restore closed");
        assert!(
            format!("{error:#}").contains("exceeded its"),
            "unexpected diagnostic: {error:#}"
        );

        let state = state.lock().unwrap();
        assert_eq!(state.events, vec!["gate-close", "link-down", "rx-barrier"]);
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
