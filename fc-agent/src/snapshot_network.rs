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
/// Whether a listener may answer a SYN with a cookie and complete the handshake from the
/// client's ACK alone. One setting for IPv4 and IPv6: `cookie_v6_check`
/// (net/ipv6/syncookies.c) reads `net->ipv4.sysctl_tcp_syncookies`, as `cookie_v4_check`
/// does. It is per network namespace, like the gate's chains and the socket dump, and all
/// three act on the agent's namespace: the container runs with `--network=host`
/// (`container.rs`), so that is where every listener of the workload is.
const SYN_COOKIES_SETTING: &str = "/proc/sys/net/ipv4/tcp_syncookies";
/// How often publication tries to write the SYN cookie setting back.
const SYN_COOKIES_WRITE_ATTEMPTS: usize = 3;

const NETLINK_SOCK_DIAG: i32 = 4;
const SOCK_DIAG_BY_FAMILY: u16 = 20;
const SOCK_DESTROY: u16 = 21;
const IPPROTO_TCP: u8 = 6;
const TCP_TIME_WAIT: u8 = 6;
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
        self.address(&self.id.destination)
    }

    fn address(&self, words: &[u32; 4]) -> Result<IpAddr> {
        match self.family as i32 {
            libc::AF_INET => Ok(IpAddr::V4(Ipv4Addr::from(words[0].to_ne_bytes()))),
            libc::AF_INET6 => {
                let mut bytes = [0u8; 16];
                for (index, word) in words.iter().enumerate() {
                    bytes[index * 4..index * 4 + 4].copy_from_slice(&word.to_ne_bytes());
                }
                Ok(IpAddr::V6(Ipv6Addr::from(bytes)))
            }
            family => bail!("unsupported socket family in snapshot manifest: {family}"),
        }
    }

    /// This socket's own end and its peer's, each an address and a port, with an
    /// IPv4-mapped address as the IPv4 address it stands for: the two sockets of one
    /// connection name the same two ends, whichever family each was opened in.
    fn ends(&self) -> Result<[(IpAddr, u16); 2]> {
        let plain = |address: IpAddr| match address {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(address, IpAddr::V4),
            v4 => v4,
        };
        Ok([
            (
                plain(self.address(&self.id.source)?),
                u16::from_be(self.id.source_port_be),
            ),
            (
                plain(self.peer_ip()?),
                u16::from_be(self.id.destination_port_be),
            ),
        ])
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

/// What a try at the boundary lock found.
enum BoundaryLock {
    /// Nobody held it; it is held here until the file is dropped.
    Free(#[allow(dead_code)] std::fs::File),
    /// A prepare, a resume or a restore is inside its boundary.
    Held,
    /// The lock could not be opened or asked. Not "held": a caller that waits for the
    /// lock to be free would otherwise wait for ever on a `/run` it cannot write.
    Unknown,
}

fn try_boundary_lock_at(path: &Path) -> BoundaryLock {
    use fs2::FileExt;
    use std::os::unix::fs::OpenOptionsExt;

    let opened = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(path)
        });
    let Ok(file) = opened else {
        return BoundaryLock::Unknown;
    };
    match FileExt::try_lock_exclusive(&file) {
        Ok(()) => BoundaryLock::Free(file),
        Err(error) if error.kind() == fs2::lock_contended_error().kind() => BoundaryLock::Held,
        Err(_) => BoundaryLock::Unknown,
    }
}

/// Who a closed gate belongs to, as two files under `/run/fcvm`. They are in guest memory,
/// so a snapshot's image holds them as its source left them.
///
/// - `owed`: a prepare closed, or was about to close, the gate, and no resume has opened
///   it since. Written by the prepare under the boundary lock before it touches the gate,
///   and removed by the resume that opens the gate. While it is there the source's
///   boundary is owed: a resume that fails leaves it, so the next one finds the boundary
///   still open to it, and an fc-agent that starts and finds it serves the host's exec at
///   once (`agent::run`), because the host's resume is the only thing that may open the
///   gate.
/// - `restore`: a restore is under way in this guest, and the gate is the restore's until
///   it publishes. Written when the restore's epoch is dispatched and removed at
///   publication. A restore that fails leaves it, and the clone powers off.
///
/// Why the guest never opens an owed gate by itself: a restored clone whose restore has
/// not been dispatched yet is, byte for byte, its source after the resume. Only the host
/// knows which of the two a guest is, and it says "a source, and its snapshot is over" by
/// running the resume one-shot.
struct BoundaryMarks {
    owed: PathBuf,
    restore: PathBuf,
}

const BOUNDARY_OWED_PATH: &str = "/run/fcvm/snapshot-boundary-owed";
const RESTORE_UNDER_WAY_PATH: &str = "/run/fcvm/restore-under-way";

impl BoundaryMarks {
    fn system() -> Self {
        Self {
            owed: PathBuf::from(BOUNDARY_OWED_PATH),
            restore: PathBuf::from(RESTORE_UNDER_WAY_PATH),
        }
    }

    fn mark(path: &Path) -> Result<()> {
        Self::write_mark(path, "")
    }

    fn write_mark(path: &Path, content: &str) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(path, content).with_context(|| format!("writing {}", path.display()))
    }

    /// Mark a source's boundary owed, with the token of the snapshot create that closed
    /// it (`BOUNDARY_TOKEN_KEY`), when the host gave one.
    fn mark_owed(&self, token: Option<&str>) -> Result<()> {
        let content = token
            .map(|token| format!("{BOUNDARY_TOKEN_KEY}{token}\n"))
            .unwrap_or_default();
        Self::write_mark(&self.owed, &content)
    }

    /// The token in the `owed` mark, if the mark is there and holds one.
    fn owed_token(&self) -> Option<String> {
        let mark = std::fs::read_to_string(&self.owed).ok()?;
        mark.lines()
            .find_map(|line| line.strip_prefix(BOUNDARY_TOKEN_KEY))
            .map(str::to_string)
    }

    /// Whether a resume that carries `token` is the resume of the boundary owed here
    /// (#2159).
    ///
    /// The host gives every snapshot create a token, passes it to the prepare, which
    /// writes it into the mark, and to every resume of that create. A resume the host
    /// gave up on can still arrive later: its exec thread goes on connecting after the
    /// create has returned, and a delivered one-shot can wait here for the boundary lock.
    /// Landing between a later create's prepare and its pause, it would open the gate and
    /// retire the manifest, and the pause would save an image with the gate open. With
    /// the token it finds another create's mark and does nothing.
    ///
    /// - A mark with a token: only the resume with that token.
    /// - A mark with none (a prepare run by hand): only a resume with none.
    /// - No mark: nothing is owed, and a resume changes nothing a boundary relies on. The
    ///   mark is the first thing a prepare writes, under the lock every resume runs
    ///   under, so there is no moment with a gate or a manifest and no mark.
    ///
    /// The token orders resumes. It is not a secret and keeps nobody out: the one-shot
    /// runs as root, and whatever can run it can read the mark.
    fn resume_is_for_the_owed_boundary(&self, token: Option<&str>) -> bool {
        !self.owed.exists() || self.owed_token().as_deref() == token
    }

    /// Write into an `owed` mark that the agent which served its prepare is gone. Only
    /// into a mark that is there, and after what it holds: this never makes one.
    fn note_agent_restarted(&self) -> Result<()> {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&self.owed)
            .and_then(|mut mark| mark.write_all(format!("{AGENT_RESTARTED_NOTE}\n").as_bytes()))
            .with_context(|| format!("writing {}", self.owed.display()))
    }

    /// Whether the agent restarted while this boundary was owed.
    fn agent_restarted(&self) -> bool {
        std::fs::read_to_string(&self.owed)
            .is_ok_and(|mark| mark.lines().any(|line| line == AGENT_RESTARTED_NOTE))
    }

    fn clear(path: &Path) -> Result<()> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| format!("removing {}", path.display())),
        }
    }

    fn restore_under_way(&self) -> bool {
        self.restore.exists()
    }

    /// A source whose gate a prepare closed and no resume has opened.
    fn source_owed(&self) -> bool {
        self.owed.exists() && !self.restore_under_way()
    }
}

/// What an agent that starts with a source's boundary owed writes into the `owed` mark.
const AGENT_RESTARTED_NOTE: &str = "agent-restarted";

/// The line of the `owed` mark that holds the token of the snapshot create which closed
/// the gate.
const BOUNDARY_TOKEN_KEY: &str = "token=";

/// The token a boundary one-shot was given: the value after `--boundary`. Letters, digits
/// and `-` only, so the mark stays one line a key.
pub fn boundary_token(args: &[String]) -> Result<Option<&str>> {
    let Some(flag) = args.iter().position(|arg| arg == "--boundary") else {
        return Ok(None);
    };
    let token = args
        .get(flag + 1)
        .map(String::as_str)
        .filter(|token| {
            !token.is_empty()
                && token.len() <= 64
                && token
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
        .context("--boundary takes a token of letters, digits and '-'")?;
    Ok(Some(token))
}

/// A resume was given the token of another snapshot create than the one whose boundary
/// is owed here, and did nothing (#2159).
#[derive(Debug)]
pub struct StaleResume;

impl std::fmt::Display for StaleResume {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "refused: this resume is not the one the boundary owed in this guest is waiting \
             for (another snapshot's token, or none where the boundary has one); nothing \
             was changed"
        )
    }
}

impl std::error::Error for StaleResume {}

/// What the resume one-shot reports, on the stderr the host reads, when the agent that
/// served the prepare died before the resume: the image the host has just taken may hold
/// a dead or starting agent and a killed container, and the host discards it
/// (`SOURCE_AGENT_RESTARTED_REPORT` in `src/commands/common.rs`; the two are held equal by
/// `the_host_reads_the_restarted_agents_report_as_the_guest_writes_it`).
pub(crate) const AGENT_RESTARTED_REPORT: &str =
    "the agent that served this snapshot's prepare died before its resume";

/// An agent that starts and finds a source's boundary owed says so in the mark, for the
/// resume that comes.
pub fn note_the_agent_restarted_in_a_boundary() {
    if let Err(error) = BoundaryMarks::system().note_agent_restarted() {
        eprintln!("[fc-agent] WARNING: {error:#}; the host will not learn that the agent restarted in the middle of its snapshot");
    }
}

/// Whether any part of the snapshot gate is installed, in either family: a gate chain,
/// or a hook that jumps to one.
async fn gate_is_installed_with<R: CommandRunner>(runner: &R) -> Result<bool> {
    for program in ["iptables", "ip6tables"] {
        let args = ["-w", "-S"];
        let output = runner
            .output(program, &args)
            .await
            .with_context(|| format!("spawning {program}"))?;
        if !output.status.success() {
            bail!(
                "snapshot network gate state read failed: {}",
                command_failure(program, &args, &output)
            );
        }
        if rules_hold_the_gate(&String::from_utf8_lossy(&output.stdout)) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether an `iptables -S` listing holds a gate chain or a jump to one: `-N <chain>`,
/// `-A <chain>`, `-j <chain>` or `-g <chain>`. A chain's name anywhere else on a line (a
/// rule's comment) is not the gate.
fn rules_hold_the_gate(rules: &str) -> bool {
    rules.lines().any(|line| {
        let words: Vec<&str> = line.split_whitespace().collect();
        words.windows(2).any(|pair| {
            matches!(pair[0], "-N" | "-A" | "-j" | "-g")
                && (pair[1] == GATE_INPUT_CHAIN || pair[1] == GATE_OUTPUT_CHAIN)
        })
    })
}

/// How often the hold on the container's start looks at the boundary.
const HOLD_LOOK: std::time::Duration = std::time::Duration::from_secs(1);

/// How many looks may find a source's gate closed with nobody working on its boundary
/// before the hold gives up: five minutes of the guest running, twice the 150 s the host
/// keeps asking a source to reopen (`SOURCE_REOPEN_BOUND` in src/commands/common.rs).
/// Looks, not a clock, as for the freeze watchdog: a guest does not run while its VM is
/// paused for the save.
const HOLD_ABANDONED_LOOKS: u32 = 300;

/// How a hold on the container's start ended.
#[derive(Debug, PartialEq, Eq)]
enum HoldEnd {
    /// No boundary held it, or the one that did is over.
    Over,
    /// A source's gate stayed closed for `HOLD_ABANDONED_LOOKS` looks with nobody working
    /// on the boundary.
    Abandoned,
}

/// Wait until no snapshot boundary holds the container's start.
///
/// The prepare freezes the container that runs, and the image is taken on the promise
/// that nothing of the workload runs behind the closed gate. An agent that starts in the
/// middle of a boundary (the one before it died after a prepare) would break that promise
/// by starting the container: on Cloud Hypervisor its boot plan comes over vsock, outside
/// the gate, and it got as far as `podman start` while the host was about to pause the
/// guest, so every clone of that image resumed with a container running behind its
/// closed gate. An agent that is not in a boundary waits for nothing.
///
/// What holds the start, one look a second:
/// - **A restore that is under way**, until it publishes. A restore that fails powers the
///   VM off, so this ends either way.
/// - **A source's gate that a prepare closed**: the `owed` mark with the gate installed.
///   The mark alone does not hold. It outlives its gate when a failed prepare's rollback
///   opened the gate and the host died before its resume, and when a resume was killed
///   between opening the gate and removing the mark, and a container must not wait for
///   ever on a mark with nothing behind it.
///
/// **When the hold gives up, and why the container then starts.** A look that finds the
/// gate closed while someone holds the boundary lock is not counted, and starts the count
/// again: a prepare or a resume is at work, and the looks counted before it belong to a
/// boundary that has moved on (without that, a count begun under one snapshot create
/// could run out between a later one's prepare and its pause). At most
/// `payload_freeze::UNCOUNTED_LOOKS_CAP` looks go uncounted, as for the freeze watchdog,
/// so a one-shot hung under the lock cannot hold the start for good.
/// `HOLD_ABANDONED_LOOKS` counted looks in a row mean no snapshot create is in flight any
/// more: the host asks a source to reopen for 150 s and then fails the create
/// and records the source gated, or the host is gone. From there nothing believes the
/// guest frozen, so the container starts behind the closed gate, with a warning, as the
/// freeze watchdog thaws one there after its own deadline. A create that comes later
/// freezes it like any running container. Not starting it would leave a guest with a
/// closed gate and no workload for good, with nothing gained: the gate still refuses
/// every new connection either way, and the resume that opens it finds the workload up.
pub async fn hold_the_container_start_through_a_boundary() {
    let runner = SystemCommandRunner;
    let end = hold_the_container_start_with(
        &BoundaryMarks::system(),
        Path::new(BOUNDARY_LOCK_PATH),
        || gate_is_installed_with(&runner),
        HOLD_LOOK,
    )
    .await;
    if end == HoldEnd::Abandoned {
        eprintln!(
            "[fc-agent] WARNING: this guest's snapshot gate has been closed for \
             {HOLD_ABANDONED_LOOKS} looks with no snapshot at work on it; starting the \
             container behind it. The gate refuses every new TCP connection until \
             `fc-agent --resume-snapshot-network` opens it"
        );
    }
}

async fn hold_the_container_start_with<F, Fut>(
    marks: &BoundaryMarks,
    lock: &Path,
    gate_is_installed: F,
    look: std::time::Duration,
) -> HoldEnd
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<bool>>,
{
    let mut announced = false;
    let mut unreadable = false;
    let mut counted = 0u32;
    let mut uncounted = 0u32;
    loop {
        let held = if marks.restore_under_way() {
            true
        } else if marks.owed.exists() {
            // A gate that cannot be read is taken for closed. Said once, not every look.
            let closed = gate_is_installed().await.unwrap_or_else(|error| {
                if !unreadable {
                    unreadable = true;
                    eprintln!(
                        "[fc-agent] WARNING: {error:#}; taking the snapshot gate for closed \
                         while it cannot be read"
                    );
                }
                true
            });
            if closed {
                let at_work = matches!(try_boundary_lock_at(lock), BoundaryLock::Held)
                    && uncounted < crate::payload_freeze::UNCOUNTED_LOOKS_CAP;
                if at_work {
                    uncounted += 1;
                    counted = 0;
                } else {
                    counted += 1;
                    if counted >= HOLD_ABANDONED_LOOKS {
                        return HoldEnd::Abandoned;
                    }
                }
            }
            closed
        } else {
            false
        };
        if !held {
            return HoldEnd::Over;
        }
        if !announced {
            announced = true;
            eprintln!(
                "[fc-agent] a snapshot boundary is in progress; the container starts when it \
                 is over"
            );
        }
        tokio::time::sleep(look).await;
    }
}

/// A one-shot was asked of a guest whose restore is under way, and did nothing: the gate,
/// the manifest and the container's freeze are the restore's until it publishes (#2138).
#[derive(Debug)]
pub struct RestoreUnderWay(&'static str);

impl std::fmt::Display for RestoreUnderWay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "refused: a restore is under way in this guest, and {} would act on the \
             restore's own boundary; nothing was changed",
            self.0
        )
    }
}

impl std::error::Error for RestoreUnderWay {}

/// A restore's epoch was dispatched: the boundary this guest was captured behind is the
/// restore's from here to its publication. Before anything else of the restore.
pub fn begin_restore_boundary() -> Result<()> {
    begin_restore_boundary_in(&BoundaryMarks::system())
}

fn begin_restore_boundary_in(marks: &BoundaryMarks) -> Result<()> {
    BoundaryMarks::mark(&marks.restore).context("marking the restore under way")?;
    // The image's `owed` is its source's. A clone owes its host no resume.
    BoundaryMarks::clear(&marks.owed)
}

/// The restore published: the guest's boundary is nobody's. A mark that is already gone
/// is removed; one that cannot be removed is the caller's to fail the restore on.
pub fn end_restore_boundary() -> Result<()> {
    end_restore_boundary_in(&BoundaryMarks::system())
}

fn end_restore_boundary_in(marks: &BoundaryMarks) -> Result<()> {
    BoundaryMarks::clear(&marks.restore)
        .context("removing the restore-under-way mark at publication")
}

/// Whether a prepare closed this guest's gate and no resume has opened it (and no restore
/// has taken the boundary over).
pub fn source_boundary_is_owed() -> bool {
    BoundaryMarks::system().source_owed()
}

/// Whether a snapshot boundary is being worked on right now: a prepare, a resume or a
/// restore holds the boundary lock, or a restore is under way. The freeze watchdog does
/// not count such a look against a freeze's deadline (#2140).
pub fn boundary_in_progress() -> bool {
    boundary_in_progress_at(&BoundaryMarks::system(), Path::new(BOUNDARY_LOCK_PATH))
}

fn boundary_in_progress_at(marks: &BoundaryMarks, lock: &Path) -> bool {
    marks.restore_under_way()
        || (lock.exists() && matches!(try_boundary_lock_at(lock), BoundaryLock::Held))
}

/// Put `rcu_expedited` back if a barrier that was killed left it on (#2141). Under the
/// boundary lock, which every barrier runs under; when someone holds it, their own
/// barrier does this first.
pub fn put_back_rcu_expedited_after_a_kill() {
    if let BoundaryLock::Free(_lock) = try_boundary_lock_at(Path::new(BOUNDARY_LOCK_PATH)) {
        ExpeditedGracePeriods::put_back(
            Path::new(RCU_EXPEDITED_KNOB),
            Path::new(RCU_EXPEDITED_RECORD),
        );
    }
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
    /// Restore side only: stop the guest's listeners completing a handshake from a SYN
    /// cookie, until `open()` publishes the clone and puts the setting back.
    ///
    /// A listener that answered a SYN with a cookie holds nothing for that client. The
    /// client's next segment is a bare ACK, which the gate passes, and the listener makes
    /// an established socket from it alone (#2123). With `net.ipv4.tcp_syncookies` at 0
    /// the listener answers that ACK with a reset, as the gate answers a SYN. The setting
    /// is the clone's own, read here and written back by `open()` in the step that
    /// publishes the clone, once the ingress half is gone. The value to write back is
    /// held by this gate, which lives with the guest-wide boundary lock from the
    /// restore's start to publication (`RestoredNetwork`), and a snapshot's prepare takes
    /// that lock first: no snapshot can be taken of a clone while its setting is 0 on
    /// the restore's account.
    ///
    /// A restore or a publication that fails leaves the setting at 0 and the clone
    /// unpublished, and nothing writes the value back then: the container can be running
    /// by then, and a cookie's ACK would reach it in a clone that failed closed. The
    /// agent powers such a clone off (`mmds.rs`, "clone restore failed closed"), so the 0
    /// serves nobody. fcvm does not refuse a snapshot asked of the clone in the seconds
    /// before the power-off lands; that image would hold the 0 together with an agent
    /// already on its way to the power-off, which a clone of it resumes.
    ///
    /// The snapshot side never calls this: a snapshot does not reconfigure its source,
    /// and a cookie handshake the source completes makes a socket that is the source's
    /// own.
    fn refuse_syn_cookies(&self) -> Result<()>;
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

/// The receive barrier: one RCU grace period. `grace_period` waits it;
/// `expedite_knob` is the kernel's `rcu_expedited` switch. Both are fields so a
/// test can watch what the barrier does with them.
struct SystemPacketReceiveBarrier {
    expedite_knob: PathBuf,
    /// Where the barrier records the switch's value before it turns it on.
    expedite_record: PathBuf,
    grace_period: fn() -> Result<()>,
}

const RCU_EXPEDITED_KNOB: &str = "/sys/kernel/rcu_expedited";
const RCU_EXPEDITED_RECORD: &str = "/run/fcvm/rcu-expedited-before";

impl SystemPacketReceiveBarrier {
    fn new() -> Self {
        Self {
            expedite_knob: PathBuf::from(RCU_EXPEDITED_KNOB),
            expedite_record: PathBuf::from(RCU_EXPEDITED_RECORD),
            grace_period: membarrier_grace_period,
        }
    }
}

/// One RCU grace period, waited in this thread. membarrier(2)'s
/// MEMBARRIER_CMD_GLOBAL is `synchronize_rcu()` in the caller, the grace period
/// `synchronize_net()` waits outside the RTNL lock: it returns once every packet
/// already in receive processing is done, so with the NEW-flow gate installed
/// first, no packet that began before it can still create an uncaptured TCP
/// socket. Merely waiting or taking repeated dumps cannot prove this.
///
/// This used to be the release of an AF_PACKET socket (`packet_release()` calls
/// `synchronize_net()`), and a release waits only when it is the last
/// reference: a process forked on another thread while the socket was open held
/// one until its exec, which then made the final release, and the barrier
/// returned without waiting.
fn membarrier_grace_period() -> Result<()> {
    const MEMBARRIER_CMD_GLOBAL: libc::c_long = 1;
    // SAFETY: membarrier(2) takes plain integer arguments.
    let result = unsafe { libc::syscall(libc::SYS_membarrier, MEMBARRIER_CMD_GLOBAL, 0, 0) };
    if result < 0 {
        return Err(io::Error::last_os_error()).context(
            "membarrier(MEMBARRIER_CMD_GLOBAL) for the receive barrier (guest kernel needs \
             CONFIG_MEMBARRIER=y and no nohz_full)",
        );
    }
    Ok(())
}

impl PacketReceiveBarrier for SystemPacketReceiveBarrier {
    fn synchronize(&self) -> Result<()> {
        let _expedited = ExpeditedGracePeriods::begin(&self.expedite_knob, &self.expedite_record);
        (self.grace_period)()
    }
}

/// The kernel's `rcu_expedited` switch held on for one barrier.
///
/// The guest kernel ticks at 100 Hz on arm64, and a normal grace period there
/// took a median 30 to 41, 37 to 46 and 57 to 66 ms at 2, 4 and 8 vCPUs in
/// restored clones on m8g.metal-24xl (0.1 to 0.2 ms expedited), on the
/// restore's path to publication and inside the source's gated window. With the
/// switch on, `synchronize_rcu()` waits an expedited grace period instead: the
/// same guarantee, reached by asking each busy CPU for a quiescent state rather
/// than waiting for ticks to report one. The switch is put back when the
/// barrier returns, and a switch that was already on is left alone; a write to
/// it by anyone else while the barrier waits is undone when it returns. A
/// switch that cannot be read or written leaves the normal grace period, which
/// is slower and just as sound.
///
/// A process killed between the two writes runs no drop guard (#2141). So the value to
/// put back is recorded in a file before the switch is turned on, and removed once it is
/// back. A record that is found says a barrier was killed with the switch on: the next
/// barrier, the next resume one-shot and the next start of the agent put the switch back
/// from it (`put_back`). The barrier never turns the switch on without the record.
///
/// Two limits. `put_back` writes the recorded value, which is 0, also when someone
/// turned the switch on on purpose between the kill and the undo: the record cannot tell
/// their 1 from the barrier's. And until one of the three runs, the 1 stays: a guest whose
/// host died with the prepare keeps expediting until its agent restarts or its next
/// snapshot.
struct ExpeditedGracePeriods<'a> {
    knob: &'a Path,
    record: &'a Path,
}

impl<'a> ExpeditedGracePeriods<'a> {
    /// Undo what a barrier that was killed left: the switch back to the recorded value,
    /// then the record gone. A failed write keeps the record for the next try.
    fn put_back(knob: &Path, record: &Path) {
        let Ok(before) = std::fs::read_to_string(record) else {
            return;
        };
        let before = match before.trim() {
            "" => "0",
            before => before,
        };
        match std::fs::write(knob, before) {
            Ok(()) => {
                eprintln!(
                    "[fc-agent] {} put back to {before}: a receive barrier was killed with it on",
                    knob.display()
                );
                let _ = std::fs::remove_file(record);
            }
            Err(error) => eprintln!(
                "[fc-agent] WARNING: cannot put {} back to {before}: {error}",
                knob.display()
            ),
        }
    }

    fn begin(knob: &'a Path, record: &'a Path) -> Option<Self> {
        Self::put_back(knob, record);
        let current = match std::fs::read_to_string(knob) {
            Ok(current) => current,
            Err(error) => {
                eprintln!(
                    "[fc-agent] WARNING: cannot read {}: {error}; the receive barrier waits a \
                     normal grace period",
                    knob.display()
                );
                return None;
            }
        };
        if current.trim() != "0" {
            return None;
        }
        let recorded = record
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(record, "0"));
        if let Err(error) = recorded {
            eprintln!(
                "[fc-agent] WARNING: cannot record {} in {}: {error}; the receive barrier \
                 waits a normal grace period",
                knob.display(),
                record.display()
            );
            return None;
        }
        match std::fs::write(knob, "1") {
            Ok(()) => Some(Self { knob, record }),
            Err(error) => {
                let _ = std::fs::remove_file(record);
                eprintln!(
                    "[fc-agent] WARNING: cannot write {}: {error}; the receive barrier waits a \
                     normal grace period",
                    knob.display()
                );
                None
            }
        }
    }
}

impl Drop for ExpeditedGracePeriods<'_> {
    fn drop(&mut self) {
        match std::fs::write(self.knob, "0") {
            Ok(()) => {
                let _ = std::fs::remove_file(self.record);
            }
            // The record stays, and the next `put_back` tries again.
            Err(error) => eprintln!(
                "[fc-agent] WARNING: cannot turn {} back off: {error}",
                self.knob.display()
            ),
        }
    }
}

/// The iptables-restore input that declares both gate chains of one family
/// with exactly the gate's rules.
fn gate_chains_restore_input(loopback_network: &str) -> String {
    let mut input = String::from("*filter\n");
    for (chain, loopback_flag) in [(GATE_INPUT_CHAIN, "-s"), (GATE_OUTPUT_CHAIN, "-d")] {
        input.push_str(&format!(":{chain} - [0:0]\n"));
        for rule in gate_chain_rules(loopback_flag, loopback_network) {
            input.push_str(&format!("-A {chain} {}\n", rule.join(" ")));
        }
    }
    input.push_str("COMMIT\n");
    input
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
    /// What iptables writes for the gate, learned at boot (`GateTemplate`).
    /// With it the restore verifies the gate and inserts the UDP rule over
    /// nfnetlink; without it, with iptables.
    template: Option<std::sync::Arc<GateTemplate>>,
    /// The kernel's SYN cookie setting, for `refuse_syn_cookies`. None leaves it alone.
    syn_cookies: Option<std::path::PathBuf>,
    /// What the setting held before `refuse_syn_cookies` wrote 0, for `open()` to put
    /// back. Guards the value only, never held across an await.
    syn_cookies_before: std::sync::Mutex<Option<String>>,
}

/// Whether `dump`, `iptables -S` in the scratch namespace after the UDP rule
/// was inserted over nfnetlink, shows the INPUT gate chain as the gate's rules
/// with `udp_ingress_rule_args` third.
fn scratch_rendering_matches(dump: &str, loopback_network: &str) -> bool {
    let mut expected = gate_chain_rules("-s", loopback_network);
    expected.insert(
        2,
        udp_ingress_rule_args(loopback_network)
            .map(str::to_string)
            .to_vec(),
    );
    parse_gate_state(dump).input_rules.is_some_and(|rules| {
        rules.len() == expected.len()
            && rules
                .iter()
                .zip(&expected)
                .all(|(observed, wanted)| sorted_tokens(observed) == sorted_tokens(wanted))
    })
}

/// The chain that holds the UDP rule while the template is learned. It exists
/// only in the scratch namespace.
const TEMPLATE_UDP_CHAIN: &str = "FCVM_SNAPSHOT_UDP";

/// The template, once boot has learned it; `None` inside when learning failed.
static GATE_TEMPLATE: std::sync::OnceLock<Option<std::sync::Arc<GateTemplate>>> =
    std::sync::OnceLock::new();

/// The template a restore uses, when boot learned one.
fn gate_template() -> Option<std::sync::Arc<GateTemplate>> {
    GATE_TEMPLATE.get().cloned().flatten()
}

/// Learn the gate template on a thread of its own, without holding up boot. A
/// restore that runs before it is learned, or after learning failed, verifies
/// the gate and adds the UDP rule with iptables, as before.
pub fn learn_gate_template_in_background() {
    let spawned = std::thread::Builder::new()
        .name("gate-template".into())
        .spawn(|| {
            let started = std::time::Instant::now();
            let learned = learn_gate_template_in_scratch_namespace();
            let template = match learned {
                Ok(template) => {
                    eprintln!(
                        "[fc-agent] learned the snapshot gate's nf_tables rules in {:.1} ms",
                        started.elapsed().as_secs_f64() * 1000.0
                    );
                    Some(std::sync::Arc::new(template))
                }
                Err(error) => {
                    eprintln!(
                        "[fc-agent] WARNING: could not learn the snapshot gate's nf_tables rules, \
                         so restores verify the gate with iptables: {error:#}"
                    );
                    None
                }
            };
            let _ = GATE_TEMPLATE.set(template);
        });
    if let Err(error) = spawned {
        eprintln!("[fc-agent] WARNING: cannot start the gate template thread: {error}");
    }
}

/// Install the gate's chains, and the UDP rule in a chain of its own, with
/// iptables-restore in a new network namespace, read back what nf_tables holds,
/// and check that the UDP rule inserted over nfnetlink renders in `iptables -S`
/// exactly as `udp_ingress_rule_args` writes it. Runs on the calling thread,
/// whose network namespace it replaces: call it only from a thread of its own.
fn learn_gate_template_in_scratch_namespace() -> Result<GateTemplate> {
    use crate::nftables::{commit, generation, insertion_batch, read_rules, Commit, Insertion};

    // SAFETY: unshare(2) takes plain flags. It moves this thread alone into a
    // new network namespace, and the processes it starts inherit it.
    if unsafe { libc::unshare(libc::CLONE_NEWNET) } != 0 {
        return Err(io::Error::last_os_error()).context("unshare(CLONE_NEWNET)");
    }
    let run = |program: &str, args: &[&str], input: Option<&str>| -> Result<String> {
        use std::io::Write;
        let mut command = std::process::Command::new(program);
        command
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = command
            .spawn()
            .with_context(|| format!("spawning {program}"))?;
        let mut stdin = child.stdin.take().context("child stdin")?;
        if let Some(input) = input {
            stdin.write_all(input.as_bytes())?;
        }
        drop(stdin);
        let output = child.wait_with_output()?;
        if !output.status.success() {
            bail!("{}", command_failure(program, args, &output));
        }
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    };
    let mut channel = crate::netlink::KernelChannel::open(
        crate::nftables::NETLINK_NETFILTER,
        "nf_tables netlink (gate template)",
    )?;
    let mut families = Vec::new();
    for (family, program, network) in [
        (Family::Ipv4, "iptables", IPV4_LOOPBACK),
        (Family::Ipv6, "ip6tables", IPV6_LOOPBACK),
    ] {
        let mut input = gate_chains_restore_input(network);
        let commit_line = input
            .rfind("COMMIT\n")
            .context("restore input has no COMMIT")?;
        input.insert_str(
            commit_line,
            &format!(
                ":{TEMPLATE_UDP_CHAIN} - [0:0]\n-A {TEMPLATE_UDP_CHAIN} {}\n",
                udp_ingress_rule_args(network).join(" ")
            ),
        );
        run(
            &format!("{program}-restore"),
            &["-w", "--noflush"],
            Some(&input),
        )?;
        let bodies = |rules: Vec<crate::nftables::ChainRule>| {
            rules.into_iter().map(|rule| rule.body).collect::<Vec<_>>()
        };
        let ingress = read_rules(&mut channel, family, GATE_INPUT_CHAIN)?;
        let egress = read_rules(&mut channel, family, GATE_OUTPUT_CHAIN)?;
        let mut udp = read_rules(&mut channel, family, TEMPLATE_UDP_CHAIN)?;
        anyhow::ensure!(
            ingress.len() == gate_chain_rules("-s", network).len()
                && egress.len() == gate_chain_rules("-d", network).len()
                && udp.len() == 1,
            "{} holds {} ingress, {} egress and {} UDP rules",
            family.name(),
            ingress.len(),
            egress.len(),
            udp.len()
        );
        let udp = udp.remove(0).body;
        let generation = generation(&mut channel)?;
        let (batch, _, end) = insertion_batch(
            &[Insertion {
                family,
                chain: GATE_INPUT_CHAIN,
                before: ingress[2].handle,
                body: &udp,
            }],
            generation,
        );
        anyhow::ensure!(
            commit(&mut channel, &batch, end)? == Commit::Committed,
            "the scratch ruleset moved under the UDP rule"
        );
        let dump = run(program, &["-w", "-S"], None)?;
        anyhow::ensure!(
            scratch_rendering_matches(&dump, network),
            "{program} renders the UDP rule inserted over nfnetlink differently:\n{dump}"
        );
        families.push(FamilyTemplate {
            input: bodies(ingress),
            output: bodies(egress),
            udp,
        });
    }
    let ipv6 = families.pop().context("no ip6 template")?;
    let ipv4 = families.pop().context("no ip template")?;
    Ok(GateTemplate {
        families: [ipv4, ipv6],
    })
}

/// What iptables-nft writes in nf_tables for the gate, per family: the rules of
/// each gate chain and the restored clone's UDP rule, as the kernel encoded
/// them. Learned once at boot, in a network namespace of its own, by running
/// the same iptables-restore input `close()` uses
/// (`learn_gate_template_in_scratch_namespace`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GateTemplate {
    /// Indexed by `family_index`.
    families: [FamilyTemplate; 2],
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FamilyTemplate {
    input: Vec<crate::nftables::RuleBody>,
    output: Vec<crate::nftables::RuleBody>,
    udp: crate::nftables::RuleBody,
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
/// per restore. With the boot-time template (`netlink`), verification is a
/// read of the gate's rules per family and the UDP rule one commit for both
/// families; without it, both are process spawns per family. Each removal is
/// a read of the rule handles and one commit for both families.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct GateTimings {
    /// Verification and the UDP rule went over nfnetlink with the boot-time
    /// template, not through iptables. The UDP rule is then one commit for
    /// both families, timed in `udp_ipv4_ms`.
    pub netlink: bool,
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
            template: None,
            syn_cookies: None,
            syn_cookies_before: std::sync::Mutex::new(None),
        }
    }

    fn with_template(mut self, template: Option<std::sync::Arc<GateTemplate>>) -> Self {
        self.template = template;
        self
    }

    /// The gate also switches SYN cookies, through the setting at `path`.
    fn with_syn_cookies(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.syn_cookies = Some(path.into());
        self
    }

    /// Write back the SYN cookie setting `refuse_syn_cookies` replaced, if it replaced one:
    /// the value read on this clone at the restore's start, whatever it was. `open` calls
    /// it once the ingress half is gone.
    ///
    /// A write that fails is tried again, and after `SYN_COOKIES_WRITE_ATTEMPTS` the clone
    /// stays published, with a warning that names the value: a clone without SYN cookies
    /// answers a SYN flood by dropping SYNs, and a clone that is taken back serves nobody.
    fn restore_syn_cookies(&self) {
        let (Some(path), Some(before)) = (
            &self.syn_cookies,
            self.syn_cookies_before.lock().unwrap().take(),
        ) else {
            return;
        };
        let mut failure = None;
        for _ in 0..SYN_COOKIES_WRITE_ATTEMPTS {
            match std::fs::write(path, &before) {
                Ok(()) => return,
                Err(error) => failure = Some(error),
            }
        }
        if let Some(error) = failure {
            eprintln!(
                "[fc-agent] WARNING: {} was not put back to {before} and stays 0; the clone \
                 is published without SYN cookies: {error}",
                path.display()
            );
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
    rules.push(owned(&TCP_NEW_FLOW_REJECT));
    rules.push(owned(&["-j", "RETURN"]));
    rules
}

/// The gate's TCP rule in both chains and both families: a SYN without ACK is
/// answered with a reset. That segment is what asks for a new TCP connection in
/// either direction, so the rule reads the packet and asks connection tracking
/// nothing.
///
/// One connection begins without that segment reaching the closed gate (#2123):
/// a listener whose SYN queue is full answers a SYN with a cookie and keeps no
/// request socket (`tcp_syncookies`), so the client's bare ACK creates the
/// established socket by itself. A cookie sent before the gate closed is
/// completed by an ACK that passes this rule. The rule does not refuse it; the
/// restore covers it. On the source the socket is the source's own, as any
/// connection it accepts is. It is in the image and in no manifest when it is
/// made after the snapshot's dump, so a restored clone lists its sockets itself
/// and retires what the manifest does not name (`unlisted_sockets`), and takes
/// no cookie from its first step until it is published
/// (`FirewallGate::refuse_syn_cookies`).
///
/// It used to be `-m conntrack --ctstate NEW`, which has two faults (#2106):
///
/// - A SYN on a tuple the guest still tracks as ESTABLISHED is not NEW. It
///   passed the closed gate, the listener accepted it, and a restored clone
///   answered a client before publication. A guest holds such entries because
///   the rule itself was what attached connection tracking: a connection open
///   when the gate's chains are removed keeps its entry with nobody tracking
///   its FIN, for the 5 days an established flow may idle, and every later
///   snapshot of that guest carries it.
/// - With tracking attached only for the gate, a segment of a connection that
///   was already open is the first the tracker sees of it, which makes it NEW:
///   on the VM being snapshotted, a live connection that carried data while
///   the gate was closed was reset.
///
/// Matching the flags closes both, and nothing in these chains uses connection
/// tracking any more, so closing the gate no longer attaches it: a snapshot
/// leaves the source's tracking table as it found it. The restored clone's UDP
/// rule (`udp_ingress_rule_args`) is the one that still does, on the restore
/// side only.
///
/// The reject stays a TCP reset in both directions. On ingress a client must
/// see a definitive refusal at once, not a SYN that times out; on egress it is
/// the reset `restore_boundary_with` describes, which is why that half opens
/// before the stale sockets are retired. The match is SYN set and ACK clear
/// whatever else is set, so a SYN that also carries FIN or RST, which a
/// listener drops anyway, is refused too. A fragment that does not hold the
/// flags does not match and passes the gate: with the nf_tables backend the
/// rule is a read of the TCP header, and a read that fails is a non-match. The
/// stack is what drops a first fragment too short for them: IPv4 reaches INPUT
/// reassembled, and IPv6 reassembly refuses a first fragment without the whole
/// TCP header.
const TCP_NEW_FLOW_REJECT: [&str; 11] = [
    "-p",
    "tcp",
    "-m",
    "tcp",
    "--tcp-flags",
    "SYN,ACK",
    "SYN",
    "-j",
    "REJECT",
    "--reject-with",
    "tcp-reset",
];

/// Ask chronyd to measure now: four polls, four answers needed. Run once the
/// gate's egress half is open, because a poll the gate rejected leaves chronyd
/// waiting its whole poll interval (up to 1024 s) to measure again.
async fn chrony_burst() {
    if let Err(error) = chronyc(&["burst", "4/4"]).await {
        eprintln!("[fc-agent] WARNING: {error:#}");
    }
}

/// Run one chronyc command and fail unless chronyd accepted it.
async fn chronyc(args: &[&str]) -> Result<()> {
    let out = tokio::process::Command::new("/usr/bin/chronyc")
        .args(args)
        .output()
        .await
        .with_context(|| format!("running chronyc {}", args.join(" ")))?;
    anyhow::ensure!(
        out.status.success(),
        "chronyc {} failed ({}): {}",
        args.join(" "),
        out.status,
        String::from_utf8_lossy(&out.stdout).trim()
    );
    Ok(())
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
///
/// What this does and does not promise. UDP has no segment that begins a flow,
/// so unlike the TCP rule (`TCP_NEW_FLOW_REJECT`) this one cannot be read off
/// the packet, and it is the one rule of the gate that needs connection
/// tracking:
///
/// - Refused until publication: a datagram that begins a flow from outside,
///   and a datagram on a flow a client began, however long ago, because a
///   stale entry for that flow still has the client as its origin.
/// - Passed: a datagram from an address and port the guest itself sent to from
///   that local port within the tracker's timeout (30 s, or 120 s once both
///   sides have sent), measured in guest run time, so an exchange shortly
///   before the snapshot counts. That is what a reply is, and the rule cannot
///   tell a late reply from a peer speaking again.
/// - Not gated at all, on either side: ICMP and every protocol other than TCP
///   and UDP. The boundary's claim is about TCP sockets (the cookie manifest
///   holds nothing else), plus this rule and the NTP one for UDP.
///
/// Because it uses tracking, the tracker is attached in a restored clone from
/// this rule's insertion until the ingress chain is removed, and connections
/// open at that moment keep their entries. The snapshot side has no such rule.
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

    /// Whether `family`'s gate is the template, read over the kept nfnetlink
    /// socket: each gate chain holds exactly the template's rules and each hook
    /// is the first rule of its built-in chain, as `family_gate_is_closed`
    /// requires of `iptables -S`.
    fn family_matches_template<C: crate::netlink::NetlinkChannel>(
        channel: &mut C,
        family: Family,
        template: &FamilyTemplate,
    ) -> Result<bool> {
        use crate::nftables::{read_rules, same_rules};
        for (hook, chain, expected) in [
            ("INPUT", GATE_INPUT_CHAIN, &template.input),
            ("OUTPUT", GATE_OUTPUT_CHAIN, &template.output),
        ] {
            let first_is_hook = match read_rules(channel, family, hook)?.first() {
                Some(rule) => rule.body.is_hook(chain)?,
                None => false,
            };
            if !first_is_hook || !same_rules(&read_rules(channel, family, chain)?, expected)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// `is_closed` over nfnetlink, both families on one socket.
    fn is_closed_over_netlink(&self, template: &GateTemplate) -> Result<bool> {
        let mut channel = self.netfilter.take()?;
        let mut closed = true;
        for family in Family::BOTH {
            let started = std::time::Instant::now();
            let matches = Self::family_matches_template(
                &mut channel,
                family,
                &template.families[family_index(family)],
            )?;
            let known = if matches {
                HalfKnown::Installed
            } else {
                HalfKnown::Unknown
            };
            self.mark(family, &[Direction::Input, Direction::Output], known);
            let elapsed = started.elapsed().as_secs_f64() * 1000.0;
            let mut timings = self.timings.lock().unwrap();
            timings.netlink = true;
            match family {
                Family::Ipv4 => timings.verify_ipv4_ms = elapsed,
                Family::Ipv6 => timings.verify_ipv6_ms = elapsed,
            }
            closed &= matches;
        }
        // Kept for the next use, never released (see `NetfilterOpener`).
        // Every error above returned with the socket dropped.
        self.netfilter.give_back(channel);
        Ok(closed)
    }

    /// The UDP rule over nfnetlink: the template's rule, ahead of the third
    /// rule of the INPUT gate chain in both families, in one commit pinned to
    /// the generation the handles were read under.
    fn gate_udp_over_netlink(&self, template: &GateTemplate) -> Result<()> {
        use crate::nftables::{commit, generation, insertion_batch, read_rules, Commit, Insertion};
        const ATTEMPTS: usize = 8;
        let started = std::time::Instant::now();
        let mut channel = self.netfilter.take()?;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let generation = generation(&mut channel)?;
            let mut insertions = Vec::new();
            for family in Family::BOTH {
                let rules = read_rules(&mut channel, family, GATE_INPUT_CHAIN)?;
                let before = rules.get(2).with_context(|| {
                    format!(
                        "the {} {GATE_INPUT_CHAIN} chain holds {} rules, too few for the UDP rule",
                        family.name(),
                        rules.len()
                    )
                })?;
                insertions.push(Insertion {
                    family,
                    chain: GATE_INPUT_CHAIN,
                    before: before.handle,
                    body: &template.families[family_index(family)].udp,
                });
            }
            let (batch, _, end) = insertion_batch(&insertions, generation);
            match commit(&mut channel, &batch, end)
                .context("adding the UDP rule to the snapshot network gate in both families")?
            {
                Commit::Committed => break,
                Commit::GenerationMoved if attempt < ATTEMPTS => continue,
                Commit::GenerationMoved => bail!(
                    "adding the UDP rule to the snapshot network gate: the ruleset changed \
                     under each of {ATTEMPTS} attempts"
                ),
            }
        }
        self.netfilter.give_back(channel);
        let mut timings = self.timings.lock().unwrap();
        timings.netlink = true;
        // One commit covers both families.
        timings.udp_ipv4_ms = started.elapsed().as_secs_f64() * 1000.0;
        timings.udp_ipv6_ms = 0.0;
        Ok(())
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
        let input = gate_chains_restore_input(loopback_network);
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
        // The setting comes back only once the ingress half is gone. A removal that fails
        // leaves the clone unpublished, and an unpublished clone takes no cookie. A SYN
        // can arrive between the two; it is answered as any other, and only one that
        // finds its listener's queue full in that moment is dropped where it would have
        // been sent a cookie, and comes again.
        self.remove_halves(&[Direction::Input, Direction::Output])
            .await?;
        self.restore_syn_cookies();
        Ok(())
    }

    fn refuse_syn_cookies(&self) -> Result<()> {
        let Some(path) = &self.syn_cookies else {
            return Ok(());
        };
        let before = match std::fs::read_to_string(path) {
            Ok(before) => before.trim().to_string(),
            // A kernel built without SYN cookies sends none.
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        };
        if before == "0" {
            return Ok(());
        }
        std::fs::write(path, "0").with_context(|| format!("writing 0 to {}", path.display()))?;
        *self.syn_cookies_before.lock().unwrap() = Some(before);
        Ok(())
    }

    async fn gate_udp_ingress(&self) -> Result<()> {
        if let Some(template) = &self.template {
            match self.gate_udp_over_netlink(template) {
                Ok(()) => return Ok(()),
                // A refused batch changed nothing, so iptables can still add
                // the rule.
                Err(error) => eprintln!(
                    "[fc-agent] WARNING: adding the UDP rule over nfnetlink failed, \
                     using iptables: {error:#}"
                ),
            }
        }
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
        if let Some(template) = &self.template {
            return self.is_closed_over_netlink(template);
        }
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
    /// Sockets retired that the manifest does not name: made behind the closed gate after
    /// the snapshot's dump (`unlisted_sockets`).
    pub(crate) unlisted: usize,
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
    /// Milliseconds waiting for the guest-wide boundary lock. Zero when the
    /// boundary ran without taking it (`restore_boundary_with` alone).
    pub lock_wait_ms: f64,
    /// Milliseconds building the gate, the receive barrier, the socket
    /// diagnostic and the manifest store, after the lock.
    pub construct_ms: f64,
    /// Milliseconds writing the cleanup-complete line to the guest console, a
    /// serial device whose write can block.
    pub complete_console_ms: f64,
    /// Milliseconds of the step the restore runs inside the boundary before
    /// egress opens: a routed clone's move to its own IPv6 address.
    pub before_egress_ms: f64,
    /// Milliseconds closing the gate when verification failed, plus adding the
    /// clone's client-direction UDP rule. The close is part of `reassert_ms`
    /// and the UDP rule is part of it too, but only on that path.
    pub gate_arm_ms: f64,
    /// Milliseconds probing the gate rules and link flags.
    pub verify_ms: f64,
    /// Milliseconds re-asserting close/link-down/barrier; zero on the
    /// verified fast path.
    pub reassert_ms: f64,
    /// Milliseconds the receive barrier took (one RCU grace period); also
    /// counted in `reassert_ms` when the gate was reinstalled.
    pub barrier_ms: f64,
    /// Milliseconds loading the manifest, listing the sockets it does not name
    /// (`unlisted_sockets`) and retiring both.
    pub destroy_ms: f64,
    /// The part of `destroy_ms` before egress opens: loading the manifest and listing
    /// the guest's TCP sockets.
    pub listing_ms: f64,
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

/// The guest's TCP sockets less the connections inside the guest that `may_stay` allows:
/// what a snapshot names in its manifest (`manifest_sockets`) and what a restore lists
/// for itself (`unlisted_sockets`).
///
/// A connection inside the guest has two sockets in the list, each the other's mirror
/// image (`TcpSocketIdentity::ends`): a loopback connection, a dual-stack server's IPv4
/// client held as an IPv4-mapped peer. A socket stays only if `may_stay` says it may and
/// its mirror image is in the list. A socket with no mirror image has its other end
/// outside the guest, or has lost it, whatever address it records for it.
///
/// One socket never stays, whatever the list holds: the server's end of a `/loopback`
/// port's client. The guest's nat rules give such a client the address 127.0.0.1 and a
/// port of `network::LOOPBACK_CLIENT_PORTS`, so the socket is the one at 127.0.0.1 and a
/// `/loopback` guest port (`SocketDiagnostic::loopback_client_ports`) whose peer is
/// 127.0.0.1 at a port of that range, and its other end is on the host. Without the
/// range the client's own source port was kept, and a client that connected from port P
/// to guest port P made a socket that is its own mirror image, and two clients crossing
/// two such ports made each other's (#2152).
///
/// Both halves of the test are needed. The peer's port alone would take every
/// connection inside the guest to a service that listens in 61000 to 65535 for a
/// client's, and reset it at each restore. What is still retired without being a client
/// from the host: a connection a guest process makes to a `/loopback` guest port from a
/// source port it bound in the range itself. That is the safe direction.
///
/// The list is two dumps, IPv4 and then IPv6, and no dump is one instant, so a connection
/// made inside the guest while it is taken can have only one end in it. When a socket
/// that may stay has no mirror image (and is not in TIME-WAIT, where no other end is
/// coming), the sockets are dumped a second time, once, and an end found there pairs it.
/// What the second dump adds is never itself listed.
fn sockets_leaving_the_guest<D: SocketDiagnostic>(
    diagnostic: &mut D,
    may_stay: impl Fn(&TcpSocketIdentity) -> Result<bool>,
) -> Result<Vec<TcpSocketIdentity>> {
    let loopback_ports = diagnostic.loopback_client_ports()?;
    let first = diagnostic.dump_tcp_sockets()?;
    let mut ends = std::collections::HashSet::with_capacity(first.len());
    for socket in &first {
        ends.insert(socket.ends()?);
    }
    // each socket that leaves, with the other end it waits for if it may stay
    let mut leaving = Vec::new();
    for socket in first {
        let [own, peer] = socket.ends()?;
        let other_end = (!is_a_loopback_ports_client(own, peer, &loopback_ports)
            && may_stay(&socket)?)
        .then_some([peer, own]);
        if !other_end.is_some_and(|end| ends.contains(&end)) {
            leaving.push((socket, other_end));
        }
    }
    let look_again = leaving
        .iter()
        .any(|(socket, other_end)| other_end.is_some() && socket.state != TCP_TIME_WAIT);
    if look_again {
        let mut seen = std::collections::HashSet::new();
        for socket in diagnostic.dump_tcp_sockets()? {
            seen.insert(socket.ends()?);
        }
        leaving.retain(|(_, other_end)| !other_end.is_some_and(|end| seen.contains(&end)));
    }
    Ok(leaving.into_iter().map(|(socket, _)| socket).collect())
}

/// Whether the socket at `own` with peer `peer` is the server's end of a `/loopback`
/// port's client: at 127.0.0.1 and one of the guest's `/loopback` ports, with a peer at
/// 127.0.0.1 and a port of the range reserved for the rewrite.
fn is_a_loopback_ports_client(
    own: (IpAddr, u16),
    peer: (IpAddr, u16),
    loopback_ports: &std::collections::HashSet<u16>,
) -> bool {
    let loopback = IpAddr::V4(Ipv4Addr::LOCALHOST);
    own.0 == loopback
        && loopback_ports.contains(&own.1)
        && peer.0 == loopback
        && crate::network::LOOPBACK_CLIENT_PORTS.contains(&peer.1)
}

/// The TCP sockets a snapshot names in its manifest, for every restore of it to retire:
/// all of them but the loopback connections inside the guest.
///
/// A loopback peer alone does not say "inside the guest". A client of a port published
/// with `/loopback` is rewritten to 127.0.0.1 on the way in
/// (`network::publish_as_loopback_client`), so the server's socket records a loopback peer
/// and is a connection from the host all the same. Going by the address left every such
/// connection of the source out of the manifest, established in every clone. So a socket
/// with a loopback peer stays out only when its mirror image is in the list too. Named
/// where the address rule left them out: that `/loopback` client's socket, and what is
/// left of a loopback connection whose other end has closed and been freed (a TIME-WAIT
/// socket, or one whose owner now reads an error where it would have read end-of-file).
///
/// A connection inside the guest over an address that is not loopback is named, both
/// ends, as it always was: a restore can take that address from the guest (a routed
/// clone moves to its own IPv6 address, `network::reconfigure_ipv6`), and the connection
/// would be left with no route between its ends.
fn manifest_sockets<D: SocketDiagnostic>(diagnostic: &mut D) -> Result<Vec<TcpSocketIdentity>> {
    sockets_leaving_the_guest(diagnostic, |socket| {
        let [_, (peer, _)] = socket.ends()?;
        Ok(peer.is_loopback())
    })
}

/// The TCP sockets a restore retires that `manifest` does not name.
///
/// The manifest is the snapshot's list. A socket made after it is in the image and in no
/// manifest, and one kind with its other end outside the guest passes the closed gate:
/// the gate refuses the SYN that asks for a connection, and a listener that answered a SYN
/// with a cookie keeps nothing, so the client's bare ACK is the whole handshake to it
/// (#2123). The source makes such a socket between its dump and its pause, where it is
/// the source's own, and the clone between its resume and the moment the restore stops
/// taking cookies (`FirewallGate::refuse_syn_cookies`).
///
/// The restore takes its list as the snapshot took its own: both halves of the gate
/// closed and receive processing drained. A socket in it that the manifest does not name
/// is retired, by cookie, with the manifest's, unless its mirror image is in the list
/// too: a connection inside the guest, which the gate passes on loopback addresses and
/// the manifest leaves out.
///
/// Not every such socket predates the restore. With the egress proxy, an outbound
/// connect is redirected to the proxy's loopback port in nat OUTPUT, before the gate's
/// OUTPUT rule sees it, so a process that runs during the restore can open a connection
/// behind the closed egress half. The container is usually not such a process: it is
/// frozen until the restore thaws it for the hook. The freeze is best effort, though
/// (`payload_freeze`: a cgroup that would not freeze, parked GPUs), so nothing here
/// relies on it. Its two sockets are not each other's mirror
/// image: the client's records the destination it asked for, the proxy's the client's
/// address. One made before the list is retired like any other socket the manifest does
/// not name, both its sockets, and the process connects again. One made between the list
/// and the opening of egress is in neither list and stays.
fn unlisted_sockets<D: SocketDiagnostic>(
    diagnostic: &mut D,
    manifest: &SnapshotNetworkManifest,
) -> Result<Vec<TcpSocketIdentity>> {
    let listed: std::collections::HashSet<[u32; 2]> = manifest
        .sockets
        .iter()
        .map(|socket| socket.id.cookie)
        .collect();
    let mut unlisted =
        sockets_leaving_the_guest(diagnostic, |socket| Ok(!listed.contains(&socket.id.cookie)))?;
    unlisted.retain(|socket| !listed.contains(&socket.id.cookie));
    Ok(unlisted)
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
    /// The guest ports published with `/loopback`, whose clients the guest's nat rules
    /// rewrite to 127.0.0.1 (`network::loopback_client_guest_ports`).
    fn loopback_client_ports(&self) -> Result<std::collections::HashSet<u16>>;
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

    fn loopback_client_ports(&self) -> Result<std::collections::HashSet<u16>> {
        crate::network::loopback_client_guest_ports()
            .context("reading which guest ports are published with /loopback")
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
    // receive barrier the second. Both are reversible and destroy no kernel
    // state: the gate is two iptables chains added then removed, the barrier one
    // expedited RCU grace period (`membarrier_grace_period`, with the kernel's
    // rcu_expedited switch on for its length and put back after).
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
    let prepared = manifest_sockets(diagnostic)
        .context("capturing cookie-bound TCP identities before snapshot");
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
    let report =
        restore_boundary_with(gate, receive_barrier, diagnostic, store, budget, || async {
            Ok(())
        })
        .await?;
    publish_with(gate, report, hook).await
}

/// The restore boundary: verify the inherited gate, drain receive processing,
/// run `before_egress`, open the gate's egress half, retire the snapshot-time
/// sockets and the manifest. The clone can reach out from the moment the
/// barrier has drained, and nothing reaches it until `publish_with` opens the
/// ingress half.
///
/// `before_egress` is where a routed clone moves to its own IPv6 address. It
/// runs once the UDP rule is in, because the new address is what lets the
/// network reach the clone, and before egress opens, so that no connection
/// leaves from the snapshot's address.
async fn restore_boundary_with<
    G: FirewallGate,
    B: PacketReceiveBarrier,
    D: SocketDiagnostic,
    S: ManifestStore,
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
>(
    gate: &G,
    receive_barrier: &B,
    diagnostic: &mut D,
    store: &S,
    budget: std::time::Duration,
    before_egress: F,
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
    // failpoint: hold before the gate is read, so a test can tamper with the restored gate
    // and drive the reinstall below in a real guest (#806).
    failpoint::hit_async("restore.pre_gate_verify").await;
    // First, before anything is read: from here to publication no listener makes a
    // socket from a SYN cookie. One made before this line is in the list taken below.
    gate.refuse_syn_cookies()
        .context("stopping SYN cookie handshakes on the restored clone")?;
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
    let gate_arm_started = std::time::Instant::now();
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
    let gate_arm_ms = gate_arm_started.elapsed().as_secs_f64() * 1000.0;
    // No link-down here either. The clone inherits a CLOSED gate inside the
    // artifact, so no new flow can appear while the recorded cookies are
    // destroyed, and the barrier below drains what was already in flight.
    // Downing the link would purge this clone's routes and neighbours, and the
    // boundary no longer captures routes to put them back.
    let barrier_started = std::time::Instant::now();
    receive_barrier
        .synchronize()
        .context("waiting for restored packet receive processing to quiesce")?;
    let barrier_ms = barrier_started.elapsed().as_secs_f64() * 1000.0;
    let reassert_ms = if verified_armed {
        0.0
    } else {
        reassert_started.elapsed().as_secs_f64() * 1000.0
    };
    let before_egress_started = std::time::Instant::now();
    before_egress()
        .await
        .context("the restore step before egress opens")?;
    let before_egress_ms = before_egress_started.elapsed().as_secs_f64() * 1000.0;
    // The restore's own list, taken while both halves of the gate are still closed
    // (`unlisted_sockets`). Its sockets are retired with the manifest's below, once
    // egress is open.
    let listing_started = std::time::Instant::now();
    let manifest = store.load()?;
    let unlisted = unlisted_sockets(diagnostic, &manifest)
        .context("listing the TCP sockets made behind the closed gate")?;
    let listing_ms = listing_started.elapsed().as_secs_f64() * 1000.0;
    // Egress opens here, before the first socket is retired. Every daemon whose
    // connection the retirement kills reconnects at once; with this half still
    // closed those connects were answered with a reset (the gate rejects with
    // tcp-reset), which clients treat as a definitive refusal: ServiceRouter
    // marked the hosts down, the configerator clients fell back to system
    // configs, and a www clone served 500s for as long as it lived. A socket a
    // daemon opens from here on is in neither list, so the cookie-bound
    // retirement below never touches it, whatever four-tuple it reuses.
    // Ingress stays closed until publication, so no published-port client
    // reaches the clone yet.
    let egress_started = std::time::Instant::now();
    gate.open_egress()
        .await
        .context("opening the egress half of the snapshot NEW-flow gate")?;
    let egress_open_ms = egress_started.elapsed().as_secs_f64() * 1000.0;
    let destroy_started = std::time::Instant::now();
    let retiring: Vec<TcpSocketIdentity> =
        manifest.sockets.iter().chain(&unlisted).copied().collect();
    let started = std::time::Instant::now();
    let mut tally = CleanupTally::default();
    let mut retired = 0;
    for batch in retiring.chunks(DESTROY_BATCH) {
        let elapsed = started.elapsed();
        if elapsed >= budget {
            bail!(
                "cookie-bound cleanup exceeded its {:?} budget after {:?} with \
                 {} of {} sockets retired; the clone stays unpublished",
                budget,
                elapsed,
                retired,
                retiring.len()
            );
        }
        let outcomes = diagnostic.destroy(batch).with_context(|| {
            format!(
                "retiring snapshot-time TCP sockets {} to {} of {}",
                retired + 1,
                retired + batch.len(),
                retiring.len()
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
            let listed = retired < manifest.sockets.len();
            retired += 1;
            match (listed, outcome) {
                (true, DestroyOutcome::Destroyed) => tally.destroyed += 1,
                (true, DestroyOutcome::AlreadyGone) => tally.already_gone += 1,
                (false, DestroyOutcome::Destroyed) => tally.unlisted += 1,
                // Closed by its owner since the listing.
                (false, DestroyOutcome::AlreadyGone) => {}
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
    let destroy_ms = listing_ms + destroy_started.elapsed().as_secs_f64() * 1000.0;
    Ok(RestoreNetworkReport {
        tally,
        verified_armed,
        lock_wait_ms: 0.0,
        construct_ms: 0.0,
        complete_console_ms: 0.0,
        gate_arm_ms,
        before_egress_ms,
        verify_ms,
        reassert_ms,
        barrier_ms,
        destroy_ms,
        listing_ms,
        egress_open_ms,
        hook_ms: 0.0,
        reopen_ms: 0.0,
        gate: gate.timings(),
    })
}

/// The restore boundary as production runs it: take the guest-wide lock,
/// build the gate, the receive barrier, the socket diagnostic and the manifest
/// store, run [`restore_boundary_with`], then announce the result on the guest
/// console. Each of those steps lies inside the restore orchestrator's
/// `tcp_cleanup_ms` and outside the boundary's own sub-timers, so each is timed
/// into the report (#1090). The lock is returned still held.
async fn restore_locked_boundary_with<G, B, D, S, F, Fut>(
    acquire_lock: impl FnOnce() -> Result<std::fs::File>,
    construct: impl FnOnce() -> (G, B, D, S),
    budget: std::time::Duration,
    before_egress: F,
    announce: impl FnOnce(&RestoreNetworkReport),
) -> Result<(std::fs::File, G, RestoreNetworkReport)>
where
    G: FirewallGate,
    B: PacketReceiveBarrier,
    D: SocketDiagnostic,
    S: ManifestStore,
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let lock_started = std::time::Instant::now();
    let lock = acquire_lock()?;
    let lock_wait_ms = lock_started.elapsed().as_secs_f64() * 1000.0;
    let construct_started = std::time::Instant::now();
    let (gate, receive_barrier, mut diagnostic, store) = construct();
    let construct_ms = construct_started.elapsed().as_secs_f64() * 1000.0;
    let mut report = restore_boundary_with(
        &gate,
        &receive_barrier,
        &mut diagnostic,
        &store,
        budget,
        before_egress,
    )
    .await?;
    let console_started = std::time::Instant::now();
    announce(&report);
    report.complete_console_ms = console_started.elapsed().as_secs_f64() * 1000.0;
    report.lock_wait_ms = lock_wait_ms;
    report.construct_ms = construct_ms;
    Ok((lock, gate, report))
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

/// What a failed prepare leaves for its caller to do.
#[derive(Debug)]
pub struct PrepareFailure {
    pub error: anyhow::Error,
    /// Whether the caller thaws the container: this prepare froze it, and the gate is
    /// open again (the prepare's own rollback removed it). False when the prepare was
    /// refused before it changed anything, and when the gate is still installed or could
    /// not be read: thawed behind a closed gate, each connection the container made was
    /// answered with a reset, and the resume the host sends after a failed prepare thaws
    /// it once the gate is open.
    pub thaw: bool,
}

/// The source side of a snapshot's boundary. `freeze` freezes the container
/// (`payload_freeze`), and runs once the boundary is this prepare's to make.
///
/// The order:
/// 1. A guest whose restore is under way refuses, before the lock and again under it:
///    its gate, its manifest and its container's freeze are the restore's (#2138). The
///    check comes before the freeze, so a refusal neither freezes a restoring clone's
///    container nor has anything to thaw.
/// 2. `owed` is written (`BoundaryMarks`): from here a resume has to open the gate.
/// 3. The container is frozen, so it opens no socket and faults no page while the
///    boundary closes, and a restored clone starts with it frozen.
/// 4. The gate, the barrier, the manifest (`prepare_with`), then the console hold.
pub async fn prepare_snapshot_network(
    token: Option<&str>,
    freeze: impl FnOnce(),
) -> std::result::Result<(), PrepareFailure> {
    prepare_snapshot_network_in(
        &BoundaryMarks::system(),
        Path::new(BOUNDARY_LOCK_PATH),
        token,
        freeze,
        || gate_is_installed_with(&SystemCommandRunner),
        || async {
            let gate = IptablesGate::new(SystemCommandRunner);
            let receive_barrier = SystemPacketReceiveBarrier::new();
            let mut diagnostic = SystemSocketDiagnostic::new();
            let captured = prepare_with(
                &gate,
                &receive_barrier,
                &mut diagnostic,
                &FileManifestStore::default(),
            )
            .await?;
            // Last, so nothing the agent logs about the boundary is still in the UART
            // when the host pauses the VM. A failure here fails the prepare, and the
            // host's recovery reopens the network gate.
            crate::console::request_source_quiesce()
                .await
                .context("holding the console quiet for the snapshot pause")?;
            eprintln!(
                "[fc-agent] snapshot network boundary prepared: gate=closed link=UNTOUCHED \
                 console=held external_socket_cookies={captured}"
            );
            Ok(())
        },
    )
    .await
}

async fn prepare_snapshot_network_in<F, Fut, G, GFut>(
    marks: &BoundaryMarks,
    lock: &Path,
    token: Option<&str>,
    freeze: impl FnOnce(),
    gate_is_installed: G,
    boundary: F,
) -> std::result::Result<(), PrepareFailure>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
    G: FnOnce() -> GFut,
    GFut: std::future::Future<Output = Result<bool>>,
{
    let refused = |error: anyhow::Error| PrepareFailure { error, thaw: false };
    if marks.restore_under_way() {
        return Err(refused(RestoreUnderWay("a snapshot's prepare").into()));
    }
    let _transaction_lock = acquire_boundary_lock_at(lock).map_err(refused)?;
    // A restore dispatched while this waited for the lock.
    if marks.restore_under_way() {
        return Err(refused(RestoreUnderWay("a snapshot's prepare").into()));
    }
    marks
        .mark_owed(token)
        .context("recording that this snapshot's boundary is owed a resume")
        .map_err(refused)?;
    freeze();
    let Err(error) = boundary().await else {
        return Ok(());
    };
    // A gate that cannot be read is taken for closed.
    let thaw = matches!(gate_is_installed().await, Ok(false));
    Err(PrepareFailure { error, thaw })
}

/// The host's clock that `--resume-snapshot-network` was given, as decimal
/// nanoseconds since the epoch: the value after `--host-time-ns`. The host passes it
/// on every reopen that follows a pause, and none on the reopen after a failed
/// prepare, which no pause preceded.
pub fn resume_host_time(args: &[String]) -> Option<&str> {
    args.iter()
        .position(|arg| arg == "--host-time-ns")
        .and_then(|flag| args.get(flag + 1))
        .map(String::as_str)
}

/// Reopen the source VM's network after a snapshot, first setting its clock to
/// `host_time` when the host passed one.
///
/// Where the VMM freezes the guest's clock while the VM is paused (the arm64 `nested`
/// profile), a resumed source is behind the host by the length of the pause.
///
/// The order around the step:
/// 1. chronyd is taken offline (`chronyc offline`) while the gate still rejects its
///    polls (`gate_chain_rules`), so it has measured nothing since the resume.
/// 2. The gate opens.
/// 3. The clock steps. A workload timer the step expires reconnects through an open
///    gate, not into the gate's TCP reset.
/// 4. chronyd comes back online and is asked for a burst, so every measurement it
///    makes after the pause is against the stepped clock and it keeps no correction
///    for the lag.
///
/// The step is a restored clone's (`bootplan::set_system_clock`), forward only
/// (`bootplan::advance_system_clock`), because the source's clock may not have stopped.
/// A clone also replaces its chronyd, because the daemon it inherits may have
/// measured a lag before the snapshot, and the source's daemon has not. A failed step
/// or chronyc call is logged and does not stop the reopen.
///
/// A reopen that fails leaves the boundary owed (#2142): the manifest and `owed` stay
/// (`BoundaryMarks`), the container stays frozen (`SourceResume::gate_open`), and the
/// one-shot exits non-zero. The host runs it again until the gate is open or its bound
/// has passed, and then says which VM is left gated.
pub async fn resume_source_network(host_time: Option<&str>, token: Option<&str>) -> SourceResume {
    let marks = BoundaryMarks::system();
    resume_source_network_in(&marks, Path::new(BOUNDARY_LOCK_PATH), token, || async {
        let gate = IptablesGate::new(SystemCommandRunner);
        let store = FileManifestStore::default();
        resume_source_network_with(&gate, &store, &marks, host_time).await
    })
    .await
}

/// The checks every resume passes before it touches anything, with the resume itself as
/// a parameter.
async fn resume_source_network_in<F, Fut>(
    marks: &BoundaryMarks,
    lock: &Path,
    token: Option<&str>,
    resume: F,
) -> SourceResume
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = SourceResume>,
{
    // Before the lock, which a restore holds until it publishes: the host's reopen after
    // a refused prepare is answered at once, not when the restore is over. And again
    // under it, for a restore dispatched meanwhile (#2138).
    if marks.restore_under_way() {
        return SourceResume::owed(RestoreUnderWay("a source's resume").into());
    }
    let _transaction_lock = match acquire_boundary_lock_at(lock) {
        Ok(lock) => lock,
        Err(error) => return SourceResume::owed(error),
    };
    if marks.restore_under_way() {
        return SourceResume::owed(RestoreUnderWay("a source's resume").into());
    }
    // Under the lock: the mark is the one the boundary now owed was given.
    if !marks.resume_is_for_the_owed_boundary(token) {
        return SourceResume::owed(StaleResume.into());
    }
    resume().await
}

/// What a source's resume did, for its caller's thaw.
#[derive(Debug)]
pub struct SourceResume {
    /// Whether the gate is open now. The container is thawed only then: thawed behind a
    /// closed gate, each connection it makes is answered with the gate's reset.
    pub gate_open: bool,
    pub result: Result<()>,
}

impl SourceResume {
    fn owed(error: anyhow::Error) -> Self {
        Self {
            gate_open: false,
            result: Err(error),
        }
    }
}

/// How many times one resume tries to open the gate, and how long it waits in between.
/// Each try is itself several attempts at the nf_tables commit. The host runs the whole
/// one-shot again after a failure, so this only spares a round trip.
const REOPEN_TRIES: u32 = 3;
const REOPEN_PAUSE: std::time::Duration = std::time::Duration::from_millis(200);

async fn resume_source_network_with<G: FirewallGate, S: ManifestStore>(
    gate: &G,
    store: &S,
    marks: &BoundaryMarks,
    host_time: Option<&str>,
) -> SourceResume {
    let started = std::time::Instant::now();
    // A barrier that was killed with `rcu_expedited` on (#2141). This one-shot holds the
    // boundary lock, which every barrier runs under.
    ExpeditedGracePeriods::put_back(
        Path::new(RCU_EXPEDITED_KNOB),
        Path::new(RCU_EXPEDITED_RECORD),
    );
    // The pause is over (or never happened, after a failed prepare). A failure only
    // leaves the agent's lines held until the hold's cap.
    if let Err(error) = crate::console::request_source_release().await {
        eprintln!("[fc-agent] WARNING: {error:#}; the agent's console stays held until its cap");
    }
    let chronyd_offline = match host_time {
        Some(_) => match chronyc(&["offline"]).await {
            Ok(()) => true,
            Err(error) => {
                eprintln!(
                    "[fc-agent] WARNING: {error:#}; stepping the clock with chronyd as it is"
                );
                false
            }
        },
        None => false,
    };
    // No routes to read back: the boundary never takes the link down, so it
    // purges nothing. The manifest is retired purely to clear the armed marker.
    //
    // ORDER: publish FIRST, retire the markers second. The gate is live traffic
    // policy on a VM that was only supposed to be READ; the manifest is a local
    // file. Retiring first meant a removal failure returned early with the gate
    // still closed, leaving the source unpublished until an operator noticed --
    // the same "snapshot disturbed the source" failure this boundary exists to
    // avoid, arriving by the error path instead.
    // A restore dispatched while this one-shot waited above takes the boundary without
    // the lock (`begin_restore_boundary`). Looked at once more, after the last wait and
    // before anything of the gate or the manifest is touched, so the two never both
    // change the boundary and the restore never waits for a resume.
    if marks.restore_under_way() {
        if chronyd_offline {
            if let Err(error) = chronyc(&["online"]).await {
                eprintln!("[fc-agent] WARNING: {error:#}; chronyd stays offline until set online");
            }
        }
        return SourceResume::owed(RestoreUnderWay("a source's resume").into());
    }
    let reopened = reopen_owed_gate(gate, REOPEN_PAUSE).await;
    if let Some(host_time) = host_time {
        crate::bootplan::advance_system_clock(host_time, started.elapsed());
    }
    if chronyd_offline {
        if let Err(error) = chronyc(&["online"]).await {
            eprintln!("[fc-agent] WARNING: {error:#}; chronyd stays offline until set online");
        }
    }
    // Read before the mark goes.
    let agent_restarted = marks.agent_restarted();
    let resumed = retire_source_boundary(reopened, store, marks);
    // A gate that is open is a source back in service, whatever became of the manifest
    // and the mark: what follows is for the running source and is done either way. The
    // caller thaws on `gate_open` and exits with `OPEN_UNCLEAN_EXIT` on the error.
    if !resumed.gate_open {
        return resumed;
    }
    eprintln!(
        "[fc-agent] source VM snapshot network reopened: gate=open \
         (link never taken down)"
    );
    if agent_restarted {
        eprintln!("[fc-agent] {AGENT_RESTARTED_REPORT}");
    }
    // A poll the gate rejected would leave chronyd waiting its whole interval.
    // Awaited: a task spawned here dies with this one-shot's runtime when it returns.
    chrony_burst().await;
    // The snapshot reset the vsock transport under the running agent's output writer,
    // which reconnects only when asked; nothing else asks a resumed source.
    if let Err(error) = crate::output::request_reconnect() {
        eprintln!(
            "[fc-agent] WARNING: {error:#}; the container's output after this snapshot \
             will not reach the host"
        );
    }
    resumed
}

/// Open the gate a prepare closed, trying `REOPEN_TRIES` times.
async fn reopen_owed_gate<G: FirewallGate>(gate: &G, pause: std::time::Duration) -> Result<()> {
    let mut reopened = Ok(());
    for attempt in 1..=REOPEN_TRIES {
        reopened = reopen_with(gate)
            .await
            .context("reopening source VM network");
        let Err(error) = &reopened else { break };
        eprintln!(
            "[fc-agent] WARNING: reopen attempt {attempt} of {REOPEN_TRIES} failed: {error:#}"
        );
        if attempt < REOPEN_TRIES {
            tokio::time::sleep(pause).await;
        }
    }
    reopened
}

/// What is left of a source's boundary after the reopen was tried.
///
/// The gate still closed: the boundary is still owed. The manifest and `owed` stay, so
/// what is left in the guest says so and the next resume starts from the same state.
/// The gate open: the manifest and `owed` go. One that cannot be removed is still an
/// error (a stale manifest keeps the restore watcher on the restore-only control
/// generation), with the source at least back in service.
fn retire_source_boundary<S: ManifestStore>(
    reopened: Result<()>,
    store: &S,
    marks: &BoundaryMarks,
) -> SourceResume {
    if let Err(reopen) = reopened {
        return SourceResume::owed(reopen.context(
            "the source's gate is still closed and its boundary is still owed: the manifest \
             is kept and the container stays frozen until a resume opens the gate",
        ));
    }
    let retired = store
        .remove()
        .context("retiring source snapshot network manifest after publication")
        .and_then(|()| BoundaryMarks::clear(&marks.owed));
    SourceResume {
        gate_open: true,
        result: retired,
    }
}

/// What a restored clone runs at publication: after the snapshot-time sockets
/// are retired and the gate's egress half is open, and before the ingress half
/// opens. A process that must redo work the snapshot made stale does it here,
/// with the network, before any published-port client can reach the clone.
pub trait RestoreHook {
    async fn run(&self) -> Result<()>;
}

/// A hook, then a step of the restore's own that must see what the hook left (a
/// service the hook restarted) and still run before ingress opens.
pub struct ThenRestoreHook<H, F>(H, F);

impl<H: RestoreHook, F: Fn() -> Result<()>> RestoreHook for ThenRestoreHook<H, F> {
    async fn run(&self) -> Result<()> {
        self.0.run().await?;
        (self.1)()
    }
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

/// How long `run_bounded` waits for a child it killed at its budget to be reaped.
/// A SIGKILLed process is normally reaped in milliseconds (the timeout test logs
/// the measured reap); one in uninterruptible sleep dies only when its I/O
/// completes, and past this bound the restore fails closed rather than wait.
const RUN_BOUNDED_REAP_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

/// How long the output of a command that has exited is still read. Its own
/// output is in the pipes by then and is read at once; the wait is only ever
/// spent when a process the command left behind holds the pipes open.
const EXITED_OUTPUT_DRAIN: std::time::Duration = std::time::Duration::from_millis(10);

/// Run `argv` with a budget. The child is killed when the budget ends (and when
/// this future is dropped), and at most `RESTORE_HOOK_OUTPUT_LIMIT` bytes of each
/// stream are kept.
///
/// The run ends when the child exits. A process the child left running keeps
/// running; the pipes it inherited are closed here, so what it writes to them
/// later fails (or ends it, by SIGPIPE), as it does to a process a `podman exec`
/// session left behind once the session has returned.
async fn run_bounded(argv: &[String], budget: std::time::Duration) -> Result<BoundedOutput> {
    run_bounded_keeping(argv, budget, RESTORE_HOOK_OUTPUT_LIMIT).await
}

/// [`run_bounded`], keeping at most `limit` bytes of each stream.
async fn run_bounded_keeping(
    argv: &[String],
    budget: std::time::Duration,
    limit: u64,
) -> Result<BoundedOutput> {
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
        let (status, streams) = {
            let reads = async {
                tokio::join!(
                    async {
                        (&mut stdout).take(limit).read_to_end(&mut out).await?;
                        tokio::io::copy(&mut stdout, &mut tokio::io::sink()).await
                    },
                    async {
                        (&mut stderr).take(limit).read_to_end(&mut err).await?;
                        tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await
                    }
                )
            };
            tokio::pin!(reads);
            // The command is over when its process exits, not when its output ends: a
            // process it left running (a hook that starts a daemon) holds the pipes open
            // for as long as it lives.
            let (exited, streams) = tokio::select! {
                exited = child.wait() => (exited, None),
                streams = &mut reads => (child.wait().await, Some(streams)),
            };
            let streams = match streams {
                Some(streams) => Some(streams),
                // What the process wrote before it exited is in the pipes already.
                None => tokio::time::timeout(EXITED_OUTPUT_DRAIN, &mut reads)
                    .await
                    .ok(),
            };
            (exited, streams)
        };
        let status = status.context("waiting for the hook")?;
        if let Some((read_out, read_err)) = streams {
            read_out.context("reading the hook's stdout")?;
            read_err.context("reading the hook's stderr")?;
        }
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
            // so nothing it spawned keeps running under the clone's name. The reap has
            // its own bound: a child stuck in uninterruptible sleep does not die on
            // SIGKILL, and an unbounded wait would leave the restore hanging with
            // ingress gated instead of failing closed.
            let pid = child.id();
            let _ = child.start_kill();
            let reap_started = std::time::Instant::now();
            let reap = match tokio::time::timeout(RUN_BOUNDED_REAP_BUDGET, child.wait()).await {
                Ok(Ok(_)) => Reap::Reaped(crate::restore::elapsed_ms(reap_started)),
                Ok(Err(error)) => Reap::WaitFailed(error),
                Err(_) => Reap::NotReaped,
            };
            let vitals = tokio::task::spawn_blocking(|| {
                format!(
                    "{}; {}",
                    crate::vitals::sample_line(),
                    crate::vitals::scheduling_line()
                )
            })
            .await
            .unwrap_or_else(|error| format!("vitals unavailable: {error}"));
            bail!("{}", timeout_message(budget, pid, reap, &vitals))
        }
    }
}

/// What happened to a child `run_bounded` killed at its budget.
enum Reap {
    /// Reaped this many milliseconds after the kill.
    Reaped(f64),
    /// Waiting for it failed; whether it still runs is not known.
    WaitFailed(std::io::Error),
    /// Not reaped within RUN_BOUNDED_REAP_BUDGET.
    NotReaped,
}

/// The error of a `run_bounded` command that ran out of `budget`.
fn timeout_message(
    budget: std::time::Duration,
    pid: Option<u32>,
    reap: Reap,
    vitals: &str,
) -> String {
    let pid = pid.map_or_else(|| "?".to_string(), |pid| pid.to_string());
    let reaped = match reap {
        Reap::Reaped(ms) => format!("killed and reaped elapsed_ms={ms:.0} later"),
        Reap::WaitFailed(error) => format!("killed pid {pid}, and waiting for it failed: {error}"),
        Reap::NotReaped => {
            format!("pid {pid} was not reaped {RUN_BOUNDED_REAP_BUDGET:?} after SIGKILL")
        }
    };
    format!("did not finish within {budget:?}; {reaped}; {vitals}")
}

/// The plan's hook, run inside the container by the container's OCI runtime
/// (`crun exec`), as the container's user, under the container's own `timeout`
/// so the command ends there when the budget does, not only its client here.
///
/// Not through podman: `podman inspect` for the container's state and `podman
/// exec` for the command are each a podman client's start-up in a just-restored
/// guest, which was most of a hooked restore's time in the guest. Both calls
/// are thin over the runtime:
///
/// - `inspect`'s `.State.Running` is podman's reading of `crun state`, which the
///   hook reads itself when podman has not reported the container running
///   ([`run_hook`]).
/// - `exec` hands `crun exec` an OCI process made from the container's own
///   (environment, working directory, user, capabilities, rlimits, labels) with
///   the command in place of the container's. crun then joins every namespace of
///   the container and its cgroup, and applies the container's seccomp filter.
///   [`exec_process`] makes that process from the config crun holds for the
///   container, and the hook hands it to the same `crun exec`.
///
/// podman builds a session's process from the spec in its database, and the
/// config crun holds is that spec after the start added to it, so two things in
/// the config are not in a session: `HOSTNAME`, unless the image or the plan
/// sets it, and some supplementary groups. [`exec_process`] takes both out as
/// [`crate::container::ExecSession`] says, from what the agent gave podman.
pub struct ContainerRestoreHook {
    pub command: String,
}

/// Whether the plan's hook has a running container to run in, read from the
/// container's runtime at publication. A pre-start snapshot restores before the
/// container exists; a snapshot taken while podman was still starting it finds
/// it not running. Neither has anything stale to redo: the container that
/// starts after the restore starts fresh.
#[derive(Debug, PartialEq, Eq)]
enum HookEligibility {
    Run,
    Skip(String),
    /// The runtime could not say, or contradicts podman's own report that the
    /// container started: the restore fails closed rather than publish a clone
    /// whose stale-state repair was silently left out.
    Fail(String),
}

/// The id of the container the hook runs in, from what the launch recorded
/// (`recorded`: the id, `None` when podman has written none, or why it could not
/// be read). With no id podman has not created the container, so the hook is
/// skipped, unless podman already reported that container started
/// (`reported_running`): then the record is wrong and the restore fails.
fn hook_container_id(
    recorded: std::result::Result<Option<String>, String>,
    reported_running: bool,
) -> std::result::Result<String, HookEligibility> {
    match recorded {
        Ok(Some(id)) if crate::container::is_container_id(id.trim()) => Ok(id.trim().to_string()),
        Ok(Some(id)) => Err(HookEligibility::Fail(format!(
            "the recorded id of fcvm-container is not a container id: {:?}",
            id.chars().take(80).collect::<String>()
        ))),
        Ok(None) if reported_running => Err(HookEligibility::Fail(
            "podman reported fcvm-container started, and no id is recorded for it".to_string(),
        )),
        Ok(None) => Err(HookEligibility::Skip(
            "podman has not created fcvm-container".to_string(),
        )),
        Err(error) => Err(HookEligibility::Fail(format!(
            "reading the id of fcvm-container: {error}"
        ))),
    }
}

/// Decide from `crun state <id>`: status `running` runs the hook; any other
/// status skips it (created and not yet started, stopped, paused); an id the
/// runtime holds no state for skips it (podman created the container and has
/// not handed it to the runtime yet, or the runtime already removed it), unless
/// podman reported the container started and its client is still running
/// (`reported_running`), which means the hook is asking the wrong runtime or
/// state root; any other failure fails the restore.
fn hook_eligibility(state: &BoundedOutput, reported_running: bool) -> HookEligibility {
    let stdout = String::from_utf8_lossy(&state.stdout);
    if state.status.success() {
        let status = serde_json::from_str::<serde_json::Value>(&stdout)
            .ok()
            .and_then(|state| Some(state.get("status")?.as_str()?.to_string()));
        return match status.as_deref() {
            Some("running") => HookEligibility::Run,
            Some(status) => {
                HookEligibility::Skip(format!("fcvm-container is not running (status={status})"))
            }
            None => {
                HookEligibility::Fail(format!("crun state printed no status: {}", stdout.trim()))
            }
        };
    }
    let stderr = String::from_utf8_lossy(&state.stderr);
    let detail = format!("crun state exited {}: {}", state.status, stderr.trim());
    // crun's two spellings of "no state for this id": 1.14 fails to open the id's
    // status file, later releases say the container does not exist.
    let unknown = (stderr.contains("/status") && stderr.contains("No such file or directory"))
        || stderr.contains("does not exist");
    match (unknown, reported_running) {
        (true, false) => HookEligibility::Skip(detail),
        (true, true) => HookEligibility::Fail(format!(
            "podman reported fcvm-container started, and the runtime has no such container; {detail}"
        )),
        (false, _) => HookEligibility::Fail(detail),
    }
}

/// How long the container-state read may take. A read that does not finish in
/// time fails the restore closed, like any other failure of the read.
///
/// The read runs at the moment a restored guest is busiest. In restores of a
/// 128 GiB www guest that is busy at snapshot time (32 vCPUs, load 107 to 128,
/// 2 to 5 percent idle, about half of the busy time in the guest kernel),
/// `podman inspect` started as the hook's read would start took 27.4 s alone and
/// 12.0 to 23.0 s while other reads and probes ran beside it, against 0.1 to
/// 0.4 s once the guest had settled; the 10 s budget this replaced failed every such restore
/// closed. The time is not mostly run-queue wait a priority could buy back: the
/// inspect used 3.6 to 9.3 s of CPU itself, three quarters of it in the kernel,
/// and `nice -n -20` took reads that ran side by side from 15 to 19 s only to 12
/// to 14 s. The budget is a little over twice the slowest read. `crun state`,
/// which does the read now, is a far smaller process than a podman client; it has
/// not been measured in such a guest, so the budget stays where podman's was.
const HOOK_INSPECT_BUDGET: std::time::Duration = std::time::Duration::from_secs(60);

/// The slowest container-state read measured in a restored guest (see
/// `HOOK_INSPECT_BUDGET`), monotonic time from spawn to exit.
#[cfg(test)]
const SLOWEST_RESTORE_TIME_INSPECT: std::time::Duration = std::time::Duration::from_millis(27_420);

/// The most of a container's `config.json` the hook reads. A config past it
/// fails the restore instead of running the hook from half of one.
const HOOK_CONFIG_LIMIT: u64 = 8 * 1024 * 1024;

/// Where the hook's OCI process is written for `crun exec --process`. A directory
/// only root writes, so nothing of the container user's is opened as root; the
/// file itself is handed to that user, whose crun reads it.
const HOOK_PROCESS_DIR: &str = "/run/fcvm-restore-hook";

/// The OCI process `crun exec` runs for the hook, as podman makes one for an exec
/// session with no user, directory or environment of its own
/// (`prepareProcessExec`, podman 4.9): the container's own process, from the
/// `config.json` its runtime holds, with
///
/// - `args` as the command and no terminal;
/// - the session's capability sets (`setProcessCapabilitiesExec`): for uid 0 the
///   container's bounding set as effective and permitted and nothing
///   inheritable; for any other user the container's effective set as effective,
///   permitted, inheritable and ambient;
/// - the supplementary groups of a session: the container's without the primary
///   group when the container was created with a user, none when it was created
///   with none, and none when that is unknown, which is never more than a
///   session has;
/// - no `HOSTNAME` when neither the image nor the plan sets one. Unknown keeps
///   the variable, which the container's own processes have.
fn exec_process(
    config: &[u8],
    args: &[String],
    session: crate::container::ExecSession,
) -> Result<String> {
    let config: serde_json::Value =
        serde_json::from_slice(config).context("parsing the container's config.json")?;
    let mut process = config
        .get("process")
        .filter(|process| process.is_object())
        .cloned()
        .context("the container's config.json holds no process")?;
    process["args"] = serde_json::json!(args);
    process["terminal"] = serde_json::json!(false);
    let uid = process
        .pointer("/user/uid")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    if let Some(caps) = process
        .get_mut("capabilities")
        .and_then(serde_json::Value::as_object_mut)
    {
        let set = |caps: &serde_json::Map<String, serde_json::Value>, name: &str| {
            caps.get(name)
                .cloned()
                .unwrap_or_else(|| serde_json::json!([]))
        };
        if uid == 0 {
            let bounding = set(caps, "bounding");
            caps.insert("effective".to_string(), bounding.clone());
            caps.insert("permitted".to_string(), bounding);
            caps.insert("inheritable".to_string(), serde_json::json!([]));
        } else {
            let effective = set(caps, "effective");
            for name in ["effective", "permitted", "inheritable", "ambient"] {
                caps.insert(name.to_string(), effective.clone());
            }
        }
    }
    if let Some(user) = process
        .get_mut("user")
        .and_then(serde_json::Value::as_object_mut)
    {
        let primary = user.get("gid").cloned();
        match (session.user_set, user.get_mut("additionalGids")) {
            (Some(true), Some(serde_json::Value::Array(groups))) => {
                groups.retain(|group| Some(group) != primary.as_ref());
            }
            _ => {
                user.remove("additionalGids");
            }
        }
    }
    if session.hostname_set == Some(false) {
        if let Some(env) = process
            .get_mut("env")
            .and_then(serde_json::Value::as_array_mut)
        {
            env.retain(|var| !var.as_str().is_some_and(|var| var.starts_with("HOSTNAME=")));
        }
    }
    Ok(process.to_string())
}

/// What the reader run as the container's user exits with when the file does
/// not exist. Not a status `env`, `runuser`, `sh` or `cat` gives for a failure
/// of their own (1, 2, 125, 126, 127).
const FILE_ABSENT_STATUS: i32 = 66;

/// Read a file the container's podman or its runtime wrote; `None` when it does
/// not exist. Root's podman writes where only root does, and the agent reads the
/// file itself. A user's podman writes in that user's runtime directory, and
/// there the read runs as that user, so the agent opens nothing as root at a
/// path the user controls.
async fn read_container_file(path: &std::path::Path, limit: u64) -> Result<Option<Vec<u8>>> {
    read_file_as(crate::container::podman_cmd_prefix(), path, limit).await
}

/// [`read_container_file`], under `prefix` (empty: read as root, directly).
/// Under a prefix the file is absent only when the reader says so with
/// [`FILE_ABSENT_STATUS`]; any other failure, a prefix that cannot run
/// included, is an error and never reads as "no such file".
async fn read_file_as(
    prefix: &[String],
    path: &std::path::Path,
    limit: u64,
) -> Result<Option<Vec<u8>>> {
    if prefix.is_empty() {
        return match tokio::fs::read(path).await {
            Ok(bytes) if bytes.len() as u64 > limit => {
                bail!("{} is over {limit} bytes", path.display())
            }
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
        };
    }
    let mut argv = prefix.to_vec();
    argv.extend(["sh".to_string(), "-c".to_string()]);
    argv.push(format!(
        r#"[ -e "$1" ] || exit {FILE_ABSENT_STATUS}; exec cat -- "$1""#
    ));
    argv.push("sh".to_string());
    argv.push(path.to_string_lossy().into_owned());
    // One byte past the limit is kept, to tell a file at the limit from one over it.
    let output = run_bounded_keeping(&argv, HOOK_INSPECT_BUDGET, limit + 1)
        .await
        .with_context(|| format!("reading {}", path.display()))?;
    if output.status.success() {
        if output.stdout.len() as u64 > limit {
            bail!("{} is over {limit} bytes", path.display());
        }
        return Ok(Some(output.stdout));
    }
    if output.status.code() == Some(FILE_ABSENT_STATUS) {
        return Ok(None);
    }
    bail!(
        "reading {}: the reader exited {}: {}",
        path.display(),
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

/// Write the hook's OCI process where `crun exec` reads it, readable by `owner`
/// (the user podman runs as) alone, or by root alone when podman runs as root.
fn write_hook_process(process: &str, owner: Option<(u32, u32)>) -> Result<std::path::PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let dir = std::path::Path::new(HOOK_PROCESS_DIR);
    match std::fs::DirBuilder::new().mode(0o755).create(dir) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error).with_context(|| format!("creating {}", dir.display())),
    }
    let path = dir.join("process.json");
    // One restore runs at a time (the boundary lock is held); a file here is a
    // killed restore's.
    let _ = std::fs::remove_file(&path);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.write_all(process.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    if let Some((uid, gid)) = owner {
        std::os::unix::fs::fchown(&file, Some(uid), Some(gid))
            .with_context(|| format!("handing {} to uid {uid}", path.display()))?;
    }
    Ok(path)
}

impl ContainerRestoreHook {
    /// `crun`, run as podman runs it: as root, or as the container's user under
    /// the prefix podman runs under, against the state root podman's crun uses.
    fn crun_argv(runtime: &crate::container::ContainerRuntime) -> Vec<String> {
        let mut argv: Vec<String> = crate::container::podman_cmd_prefix().to_vec();
        argv.extend(["crun".to_string(), "--root".to_string()]);
        argv.push(runtime.crun_root.to_string_lossy().into_owned());
        argv
    }

    fn state_argv(runtime: &crate::container::ContainerRuntime, id: &str) -> Vec<String> {
        let mut argv = Self::crun_argv(runtime);
        argv.extend(["state".to_string(), id.to_string()]);
        argv
    }

    fn exec_argv(
        runtime: &crate::container::ContainerRuntime,
        id: &str,
        process: &std::path::Path,
    ) -> Vec<String> {
        let mut argv = Self::crun_argv(runtime);
        argv.extend(["exec".to_string(), "--process".to_string()]);
        argv.push(process.to_string_lossy().into_owned());
        argv.push(id.to_string());
        argv
    }

    /// The command as the container runs it: under `timeout`, through `sh -c`.
    fn args(&self) -> Vec<String> {
        let mut args: Vec<String> = ["timeout", "-k"].into_iter().map(str::to_string).collect();
        args.push(RESTORE_HOOK_KILL_AFTER_SECS.to_string());
        args.push(RESTORE_HOOK_COMMAND_SECS.to_string());
        args.extend(["sh", "-c"].into_iter().map(str::to_string));
        args.push(self.command.clone());
        args
    }

    /// Run the command in the container `id` names: `crun exec` of the process
    /// made from the config crun holds for it.
    async fn exec(
        &self,
        runtime: &crate::container::ContainerRuntime,
        id: &str,
    ) -> Result<BoundedOutput> {
        let config_path = runtime.crun_root.join(id).join("config.json");
        let config = read_container_file(&config_path, HOOK_CONFIG_LIMIT)
            .await?
            .with_context(|| format!("{} does not exist", config_path.display()))?;
        let process = exec_process(&config, &self.args(), runtime.session)?;
        let process_file = write_hook_process(&process, runtime.owner)?;
        let output = run_bounded(
            &Self::exec_argv(runtime, id, &process_file),
            RESTORE_HOOK_BUDGET,
        )
        .await;
        let _ = std::fs::remove_file(&process_file);
        output
    }
}

/// What the hook asks of the container's runtime: crun in the agent, a fake in
/// the tests of [`run_hook`].
trait HookRuntime {
    /// `crun state`: whether the hook has a running container.
    async fn state(&self) -> Result<HookEligibility>;
    /// `crun exec` of the hook's command.
    async fn exec(&self) -> Result<BoundedOutput>;
}

/// crun, for the container `id` names.
struct Crun<'a> {
    hook: &'a ContainerRestoreHook,
    runtime: &'a crate::container::ContainerRuntime,
    id: &'a str,
}

impl HookRuntime for Crun<'_> {
    async fn state(&self) -> Result<HookEligibility> {
        // Timed either way: a read that ran out says how long it waited, and one that
        // finished gives the healthy distribution to compare a starved one against.
        let read_started = std::time::Instant::now();
        let state = run_bounded(
            &ContainerRestoreHook::state_argv(self.runtime, self.id),
            HOOK_INSPECT_BUDGET,
        )
        .await
        .with_context(|| {
            format!(
                "reading fcvm-container's state before the restore hook (elapsed_ms={:.0})",
                crate::restore::elapsed_ms(read_started)
            )
        })?;
        eprintln!(
            "[fc-agent] crun state before the restore hook returned code={} elapsed_ms={:.0}",
            state
                .status
                .code()
                .map_or_else(|| "signal".to_string(), |code| code.to_string()),
            crate::restore::elapsed_ms(read_started)
        );
        // Read only when podman has not reported the container running.
        Ok(hook_eligibility(&state, false))
    }

    async fn exec(&self) -> Result<BoundedOutput> {
        self.hook.exec(self.runtime, self.id).await
    }
}

/// What a `crun exec` that did not succeed says of the container, when it is
/// crun refusing before it starts the command and not the command failing.
#[derive(Debug, PartialEq, Eq)]
enum ExecRefusal {
    /// The container is stopped or paused: there is nothing to run the hook in.
    NotRunning(String),
    /// crun holds no container of that id.
    NoSuchContainer(String),
}

/// crun's own refusal, read from an exec that exited 1 having printed nothing
/// but crun's one line. The lines name the container's id (64 hex digits), which
/// no hook prints by accident, so a hook that fails with a message of its own
/// about something "not running" is a hook that failed. crun 1.14 checks all
/// three before it starts the command: a paused container is refused at once
/// (8 ms measured), not entered and frozen.
fn exec_refusal(output: &BoundedOutput, id: &str) -> Option<ExecRefusal> {
    if output.status.code() != Some(1) || !output.stdout.is_empty() {
        return None;
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let said = stderr.trim();
    if said.lines().count() != 1 {
        return None;
    }
    let detail = format!("crun exec exited {}: {said}", output.status);
    if said.ends_with(&format!("the container `{id}` is not running"))
        || said.ends_with(&format!("the container `{id}` is paused"))
    {
        return Some(ExecRefusal::NotRunning(detail));
    }
    // crun's two spellings of "no state for this id": 1.14 fails to open the id's
    // status file, later releases say the container does not exist.
    if (said.contains(&format!("/{id}/status`")) && said.ends_with("No such file or directory"))
        || said.ends_with(&format!("container `{id}` does not exist"))
    {
        return Some(ExecRefusal::NoSuchContainer(detail));
    }
    None
}

/// Run the hook in the container `id` names; its output, or `None` when it has
/// no running container to run in (the reason is logged). An output whose
/// status is a failure is the hook's own: it ran, and whatever the container
/// does afterwards the restore fails.
///
/// When podman has reported the container started and its client is still
/// attached (`reported_running`), the command is run at once: the container was
/// running when the snapshot was taken, a restore resumes it as it was, and
/// `crun exec` itself refuses a container that is not running, so a state read
/// first would start one more process in a just-restored guest to learn what
/// the exec is about to say. What crun refuses decides as the state read did:
/// a container that is stopped or paused skips the hook, and a runtime that
/// holds no such container fails the restore, because podman said it runs.
///
/// Without podman's report (a snapshot taken while the container was starting,
/// or after it exited) the state is read first, and only `running` runs the
/// hook: the report alone never runs or skips one.
async fn run_hook(
    runtime: &impl HookRuntime,
    id: &str,
    reported_running: bool,
) -> Result<Option<BoundedOutput>> {
    if !reported_running {
        return match runtime.state().await? {
            HookEligibility::Run => Ok(Some(runtime.exec().await?)),
            HookEligibility::Skip(reason) => {
                eprintln!("[fc-agent] restore hook skipped: {reason}");
                Ok(None)
            }
            HookEligibility::Fail(reason) => {
                bail!("cannot tell whether the container runs; {reason}")
            }
        };
    }
    let output = runtime.exec().await?;
    if output.status.success() {
        return Ok(Some(output));
    }
    match exec_refusal(&output, id) {
        Some(ExecRefusal::NotRunning(reason)) => {
            eprintln!("[fc-agent] restore hook skipped: fcvm-container is not running; {reason}");
            Ok(None)
        }
        Some(ExecRefusal::NoSuchContainer(reason)) => bail!(
            "cannot tell whether the container runs; podman reported fcvm-container \
             started, and the runtime has no such container; {reason}"
        ),
        None => Ok(Some(output)),
    }
}

impl RestoreHook for ContainerRestoreHook {
    async fn run(&self) -> Result<()> {
        // No record: the agent has not come to launching the container in this boot,
        // and nothing else starts one.
        let Some(runtime) = crate::container::container_runtime() else {
            eprintln!("[fc-agent] restore hook skipped: the agent has not launched fcvm-container");
            return Ok(());
        };
        let reported_running = crate::container::container_reported_running();
        let recorded = match crate::container::captured_container_id() {
            Some(id) => Ok(Some(id.to_string())),
            None => read_container_file(&runtime.id_file, 256)
                .await
                .map(|id| id.map(|id| String::from_utf8_lossy(&id).into_owned()))
                .map_err(|error| format!("{error:#}")),
        };
        let id = match hook_container_id(recorded, reported_running) {
            Ok(id) => id,
            Err(HookEligibility::Skip(reason)) => {
                eprintln!("[fc-agent] restore hook skipped: {reason}");
                return Ok(());
            }
            Err(HookEligibility::Fail(reason)) => bail!(
                "restore hook {:?} cannot tell whether the container runs; {reason}",
                self.command
            ),
            Err(HookEligibility::Run) => unreachable!("an id is not a decision to run"),
        };
        let started = std::time::Instant::now();
        let crun = Crun {
            hook: self,
            runtime,
            id: &id,
        };
        let Some(output) = run_hook(&crun, &id, reported_running)
            .await
            .with_context(|| format!("restore hook {:?}", self.command))?
        else {
            return Ok(());
        };
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

    /// Run the plan's restore hook, if any, then `after_hook`, then open ingress: the
    /// clone is published. A hook or `after_hook` failure keeps it unpublished and
    /// fails the restore.
    pub async fn publish(
        self,
        hook: Option<&str>,
        after_hook: impl Fn() -> Result<()>,
    ) -> Result<RestoreNetworkReport> {
        let report = match hook {
            Some(command) => {
                let hook = ThenRestoreHook(
                    ContainerRestoreHook {
                        command: command.to_string(),
                    },
                    after_hook,
                );
                publish_with(&self.gate, self.report, &hook).await?
            }
            None => {
                let hook = ThenRestoreHook(NoRestoreHook, after_hook);
                publish_with(&self.gate, self.report, &hook).await?
            }
        };
        eprintln!(
            "[fc-agent] snapshot network published: destroyed={} already_gone={} \
             verified_armed={} link=UNTOUCHED gate=open",
            report.tally.destroyed, report.tally.already_gone, report.verified_armed
        );
        Ok(report)
    }
}

/// `before_egress` runs inside the boundary, after the clone's UDP rule and
/// before egress opens (`restore_boundary_with`).
pub async fn restore_snapshot_network<F, Fut>(before_egress: F) -> Result<RestoredNetwork>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let (transaction_lock, gate, report) = restore_locked_boundary_with(
        acquire_boundary_lock,
        || {
            (
                IptablesGate::new(SystemCommandRunner)
                    .with_template(gate_template())
                    .with_syn_cookies(SYN_COOKIES_SETTING),
                SystemPacketReceiveBarrier::new(),
                SystemSocketDiagnostic::new(),
                FileManifestStore::default(),
            )
        },
        CLEANUP_BUDGET,
        before_egress,
        |report| {
            eprintln!(
                "[fc-agent] snapshot network cleanup complete: destroyed={} already_gone={} \
                 verified_armed={} link=UNTOUCHED gate=egress-open unlisted={} listing_ms={:.3}",
                report.tally.destroyed,
                report.tally.already_gone,
                report.verified_armed,
                report.tally.unlisted,
                report.listing_ms
            );
        },
    )
    .await?;
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

    /// The socket at `own` of a connection to `peer`: IPv6 when its own address is, IPv4
    /// otherwise.
    fn end_at(cookie: u32, own: (IpAddr, u16), peer: (IpAddr, u16)) -> TcpSocketIdentity {
        let words = |address: IpAddr| match address {
            IpAddr::V4(v4) => [u32::from_ne_bytes(v4.octets()), 0, 0, 0],
            IpAddr::V6(v6) => {
                let bytes = v6.octets();
                let word =
                    |i: usize| u32::from_ne_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap());
                [word(0), word(1), word(2), word(3)]
            }
        };
        TcpSocketIdentity {
            family: if own.0.is_ipv6() {
                libc::AF_INET6
            } else {
                libc::AF_INET
            } as u8,
            state: 1,
            id: InetDiagSockId {
                source_port_be: own.1.to_be(),
                destination_port_be: peer.1.to_be(),
                source: words(own.0),
                destination: words(peer.0),
                interface_id: 0,
                cookie: [cookie, 0],
            },
        }
    }

    /// The two sockets of one connection inside the guest, a client on port 40000 at
    /// `client` and the server's accepted socket on port 8080 at `server`.
    fn connection(
        client_cookie: u32,
        server_cookie: u32,
        client: &str,
        server: &str,
    ) -> [TcpSocketIdentity; 2] {
        let client: (IpAddr, u16) = (client.parse().unwrap(), 40000);
        let server: (IpAddr, u16) = (server.parse().unwrap(), 8080);
        [
            end_at(client_cookie, client, server),
            end_at(server_cookie, server, client),
        ]
    }

    /// One the guest made to its own address, 10.0.2.100.
    fn in_guest_connection(client_cookie: u32, server_cookie: u32) -> [TcpSocketIdentity; 2] {
        connection(client_cookie, server_cookie, "10.0.2.100", "10.0.2.100")
    }

    /// The same socket as a dual-stack server holds it: IPv6, its addresses IPv4-mapped.
    fn as_dual_stack(socket: TcpSocketIdentity) -> TcpSocketIdentity {
        let mapped = |address: IpAddr| match address {
            IpAddr::V4(v4) => IpAddr::V6(v4.to_ipv6_mapped()),
            v6 => v6,
        };
        let [own, peer] = socket.ends().unwrap();
        end_at(
            socket.id.cookie[0],
            (mapped(own.0), own.1),
            (mapped(peer.0), peer.1),
        )
    }

    /// The accepted socket of a client of a port published with `/loopback`: the client is
    /// on the host, and the guest's nat rules rewrote both its addresses to 127.0.0.1 and
    /// its port to one of `network::LOOPBACK_CLIENT_PORTS`
    /// (`network::publish_as_loopback_client`). No socket in the guest is its other end.
    fn loopback_client_port_socket(cookie: u32) -> TcpSocketIdentity {
        let loopback = IpAddr::V4(Ipv4Addr::LOCALHOST);
        end_at(cookie, (loopback, LOOPBACK_PORT), (loopback, 61000))
    }

    /// A `/loopback` port's client is outside the guest whatever the socket list holds
    /// (#2152). Its socket has a 127.0.0.1 peer, and the list can hold that socket's
    /// mirror image without the connection being inside the guest:
    ///
    /// - a client of guest port P whose source port is P is its own mirror image;
    /// - two clients crossing two `/loopback` ports, P1 from source port P2 and P2 from
    ///   source port P1, are each other's;
    /// - what a closed connection inside the guest left, a client's socket from port x
    ///   whose server end is gone, is the mirror image of a later `/loopback` client that
    ///   arrives from port x.
    ///
    /// The rewrite gives such a client a port of a reserved range, and the socket at a
    /// `/loopback` guest port whose peer is 127.0.0.1 at a port of that range never stays.
    ///
    /// The socket's own port is half of the test. A service of the guest that listens on
    /// 127.0.0.1 in the reserved range, with a client inside the guest, is a connection
    /// inside the guest and stays.
    ///
    /// RED BEFORE THE FIX: each of the three shapes was kept by the manifest and by the
    /// restore.
    /// RED WITH THE PEER'S PORT ALONE: the client's end of a connection inside the guest
    /// to a service listening on 127.0.0.1:61000 is named and reset in every clone.
    #[test]
    fn a_loopback_ports_client_never_stays_whatever_the_list_holds() {
        let lo = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let p1 = *crate::network::LOOPBACK_CLIENT_PORTS.start();
        let p2 = p1 + 1;
        let empty = SnapshotNetworkManifest {
            version: MANIFEST_VERSION,
            routes: Vec::new(),
            sockets: Vec::new(),
        };
        let both = |published: &[u16],
                    sockets: &[TcpSocketIdentity],
                    leaving: &[TcpSocketIdentity]| {
            assert_eq!(
                manifest_sockets(&mut Dumps::of(&[sockets]).publishing(published)).unwrap(),
                leaving.to_vec(),
                "the manifest"
            );
            assert_eq!(
                unlisted_sockets(&mut Dumps::of(&[sockets]).publishing(published), &empty).unwrap(),
                leaving.to_vec(),
                "the restore's list"
            );
        };
        // its own mirror image: `/loopback` guest port P, from source port P
        let own_image = end_at(1, (lo, p1), (lo, p1));
        both(&[p1, p2], &[own_image], &[own_image]);
        // two clients, two `/loopback` ports, each the other's mirror image
        let crossing = [end_at(2, (lo, p1), (lo, p2)), end_at(3, (lo, p2), (lo, p1))];
        both(&[p1, p2], &crossing, &crossing);
        // a leaked in-guest client end that a later `/loopback` client happens to mirror
        let leaked = end_at(4, (lo, p2), (lo, LOOPBACK_PORT));
        let later_client = end_at(5, (lo, LOOPBACK_PORT), (lo, p2));
        both(&[LOOPBACK_PORT], &[leaked, later_client], &[later_client]);

        // A connection inside the guest from an ephemeral port stays, as before.
        let inside = connection(6, 7, "127.0.0.1", "127.0.0.1");
        both(&[LOOPBACK_PORT], &inside, &[]);
        assert!(!crate::network::LOOPBACK_CLIENT_PORTS.contains(&40000));
        // A service of the guest that listens in the reserved range, on a port that is
        // not published with `/loopback`, and its client inside the guest: stays.
        let service = [
            end_at(8, (lo, 40000), (lo, p1)),
            end_at(9, (lo, p1), (lo, 40000)),
        ];
        both(&[LOOPBACK_PORT], &service, &[]);
        both(&[], &service, &[]);
        // And the same two sockets when p1 is a `/loopback` port: the client's port is
        // not of the range, so this is no rewritten client either.
        both(&[p1], &service, &[]);

        let ports: std::collections::HashSet<u16> = [LOOPBACK_PORT].into_iter().collect();
        let client =
            |own: (IpAddr, u16), peer: (IpAddr, u16)| is_a_loopback_ports_client(own, peer, &ports);
        assert!(client((lo, LOOPBACK_PORT), (lo, 65535)));
        assert!(!client((lo, LOOPBACK_PORT), (lo, 60999)));
        assert!(!client((lo, 8080), (lo, p1)), "not a /loopback guest port");
        // only 127.0.0.1 is the rewrite's address, on both ends
        assert!(!client(
            (lo, LOOPBACK_PORT),
            ("127.0.0.53".parse().unwrap(), p1)
        ));
        assert!(!client((lo, LOOPBACK_PORT), ("::1".parse().unwrap(), p1)));
        assert!(!client(
            ("10.0.2.100".parse().unwrap(), LOOPBACK_PORT),
            (lo, p1)
        ));
    }

    /// A diagnostic that answers each dump with the next of the lists it was given, and
    /// the last of them from then on.
    struct Dumps {
        lists: Vec<Vec<TcpSocketIdentity>>,
        taken: usize,
        /// The guest ports published with `/loopback`.
        loopback_ports: Vec<u16>,
    }

    /// The `/loopback` guest port of the test doubles' guest.
    const LOOPBACK_PORT: u16 = 8097;

    impl Dumps {
        fn of(lists: &[&[TcpSocketIdentity]]) -> Dumps {
            Dumps {
                lists: lists.iter().map(|list| list.to_vec()).collect(),
                taken: 0,
                loopback_ports: vec![LOOPBACK_PORT],
            }
        }

        fn publishing(mut self, loopback_ports: &[u16]) -> Dumps {
            self.loopback_ports = loopback_ports.to_vec();
            self
        }
    }

    impl SocketDiagnostic for Dumps {
        fn dump_tcp_sockets(&mut self) -> Result<Vec<TcpSocketIdentity>> {
            let list = self.lists[self.taken.min(self.lists.len() - 1)].clone();
            self.taken += 1;
            Ok(list)
        }

        fn loopback_client_ports(&self) -> Result<std::collections::HashSet<u16>> {
            Ok(self.loopback_ports.iter().copied().collect())
        }

        fn destroy(&mut self, _sockets: &[TcpSocketIdentity]) -> Result<Vec<DestroyOutcome>> {
            bail!("this diagnostic only lists")
        }

        fn destroy_datagrams(&self) -> usize {
            0
        }
    }

    fn socket6(peer: Ipv6Addr) -> TcpSocketIdentity {
        let words = peer.octets();
        let word = |i: usize| u32::from_ne_bytes(words[i * 4..i * 4 + 4].try_into().unwrap());
        TcpSocketIdentity {
            family: libc::AF_INET6 as u8,
            state: 1,
            id: InetDiagSockId {
                source_port_be: 25565u16.to_be(),
                destination_port_be: 54848u16.to_be(),
                source: [0, 0, 0, 0],
                destination: [word(0), word(1), word(2), word(3)],
                interface_id: 0,
                cookie: [7, 0],
            },
        }
    }

    /// A loopback connection with both its sockets in the guest stays out of the manifest,
    /// in every form the kernel reports one: IPv4, IPv6, and a dual-stack server (Pumpkin in
    /// Minecraft mode) holding its IPv4 loopback client as an IPv6 socket with IPv4-mapped
    /// addresses. Everything else is named, whatever address it records for its peer.
    ///
    /// RED WITH THE ADDRESS RULE: the `/loopback` port's client is left out for its
    /// 127.0.0.1 peer, and so is the socket whose other end is gone.
    /// RED WITH THE MIRROR IMAGE ALONE: the connection over the guest's own address is
    /// left out, and a restore that takes the address from the guest leaves it hanging.
    #[test]
    fn the_manifest_leaves_out_loopback_connections_inside_the_guest_and_nothing_else() {
        let named =
            |sockets: &[TcpSocketIdentity]| manifest_sockets(&mut Dumps::of(&[sockets])).unwrap();
        let [client, server] = connection(1, 2, "127.0.0.1", "127.0.0.1");
        assert_eq!(named(&[client, server]), vec![]);
        assert_eq!(named(&connection(1, 2, "127.0.0.1", "127.0.0.53")), vec![]);
        assert_eq!(named(&connection(1, 2, "::1", "::1")), vec![]);
        // retiring the dual-stack server's end reset every in-guest client of it
        assert_eq!(named(&[client, as_dual_stack(server)]), vec![]);
        assert_eq!(
            named(&[as_dual_stack(client), as_dual_stack(server)]),
            vec![]
        );

        let outside = [
            socket(3, [10, 0, 2, 2]),
            socket6("::ffff:10.0.2.2".parse().unwrap()),
            socket6("2001:db8::1".parse().unwrap()),
            socket6("fe80::1".parse().unwrap()),
        ];
        assert_eq!(named(&outside), outside.to_vec());
        // #2124's review: a loopback peer is not a connection inside the guest. A client
        // of a `/loopback` port is on the host.
        let from_the_host = loopback_client_port_socket(4);
        assert_eq!(named(&[client, server, from_the_host]), vec![from_the_host]);
        // one end closed and freed: what is left is no connection
        assert_eq!(named(&[client]), vec![client]);
        // over the guest's own address, which a restore can take away: named, as on main
        let own_address = in_guest_connection(5, 6);
        assert_eq!(named(&own_address), own_address.to_vec());
        let routed = connection(7, 8, "2001:db8::100", "2001:db8::100");
        assert_eq!(named(&routed), routed.to_vec());
    }

    /// No dump is one instant, so a connection made inside the guest while the list is
    /// taken can have one end in it. A socket that may stay and has no mirror image is
    /// looked for once more, and the end the second dump shows pairs it. A socket that
    /// leaves anyway costs no second dump, nor does one in TIME-WAIT, and what only the
    /// second dump shows is not itself listed.
    ///
    /// RED WITH ONE DUMP: the dual-stack server's end of a new loopback connection is
    /// named because the IPv4 dump was taken before its client existed.
    #[test]
    fn an_end_the_first_dump_missed_is_looked_for_once_more() {
        let [client, server] = connection(1, 2, "127.0.0.1", "127.0.0.1");
        let server = as_dual_stack(server);
        let outside = socket(3, [198, 51, 100, 8]);
        let newer = socket(4, [198, 51, 100, 9]);

        let mut dumps = Dumps::of(&[&[server, outside], &[client, server, outside, newer]]);
        assert_eq!(manifest_sockets(&mut dumps).unwrap(), vec![outside]);
        assert_eq!(dumps.taken, 2);

        // the other end is not there the second time either: it is gone
        let mut dumps = Dumps::of(&[&[server, outside]]);
        assert_eq!(manifest_sockets(&mut dumps).unwrap(), vec![server, outside]);
        assert_eq!(dumps.taken, 2);

        // nothing that may stay is without its other end: one dump
        let mut dumps = Dumps::of(&[&[client, server, outside]]);
        assert_eq!(manifest_sockets(&mut dumps).unwrap(), vec![outside]);
        assert_eq!(dumps.taken, 1);
        let own_address = in_guest_connection(5, 6)[1];
        let mut dumps = Dumps::of(&[&[own_address, outside]]);
        assert_eq!(
            manifest_sockets(&mut dumps).unwrap(),
            vec![own_address, outside]
        );
        assert_eq!(dumps.taken, 1);
        let closed = TcpSocketIdentity {
            state: TCP_TIME_WAIT,
            ..connection(7, 8, "127.0.0.1", "127.0.0.1")[0]
        };
        let mut dumps = Dumps::of(&[&[closed, outside]]);
        assert_eq!(manifest_sockets(&mut dumps).unwrap(), vec![closed, outside]);
        assert_eq!(dumps.taken, 1);

        // The restore's list: a socket the manifest names leaves anyway, and one it does
        // not name is looked for again whatever its addresses.
        let manifest = SnapshotNetworkManifest {
            version: MANIFEST_VERSION,
            routes: Vec::new(),
            sockets: vec![outside],
        };
        let mut dumps = Dumps::of(&[&[outside]]);
        assert_eq!(unlisted_sockets(&mut dumps, &manifest).unwrap(), vec![]);
        assert_eq!(dumps.taken, 1);
        let mut dumps = Dumps::of(&[&[server, outside], &[client, server, outside, newer]]);
        assert_eq!(unlisted_sockets(&mut dumps, &manifest).unwrap(), vec![]);
        assert_eq!(dumps.taken, 2);
        let mut dumps = Dumps::of(&[&[outside, newer]]);
        assert_eq!(
            unlisted_sockets(&mut dumps, &manifest).unwrap(),
            vec![newer]
        );
        assert_eq!(dumps.taken, 2);
    }

    /// A loopback socket with no mirror image is named and retired whatever its state,
    /// also where it is one end of a connection inside the guest that the kernel left
    /// alone. That happens two ways. The other end closed and its TIME-WAIT socket
    /// expired, which leaves the survivor in CLOSE-WAIT. Or a SYN to a loopback listener
    /// whose accept queue is full was dropped, which leaves the client alone in SYN-SENT.
    /// A process of the clone that holds such an end made after the resume reads a reset
    /// where its peer was already gone, or has one connect fail that would have sent its
    /// SYN again.
    ///
    /// The state cannot excuse the socket. A `/loopback` port's client that has sent its
    /// FIN leaves the server's socket in CLOSE-WAIT with a 127.0.0.1 peer and no mirror
    /// image, and the server can still send to that client on the host. The two are the
    /// same record in a dump, so a rule that kept the first would keep the second.
    ///
    /// This pins what the code does, so that changing it is a decision.
    #[test]
    fn a_lone_loopback_end_is_named_and_retired_whatever_its_state() {
        const TCP_SYN_SENT: u8 = 2;
        const TCP_CLOSE_WAIT: u8 = 8;
        let empty = SnapshotNetworkManifest {
            version: MANIFEST_VERSION,
            routes: Vec::new(),
            sockets: Vec::new(),
        };
        for state in [TCP_CLOSE_WAIT, TCP_SYN_SENT] {
            let [client, server] = connection(1, 2, "127.0.0.1", "127.0.0.1");
            let survivor = TcpSocketIdentity { state, ..server };
            let connecting = TcpSocketIdentity { state, ..client };
            let from_the_host = TcpSocketIdentity {
                state,
                ..loopback_client_port_socket(3)
            };
            // The `/loopback` client is known by its port (#2152) and costs no second
            // look; the other two are looked for once more first.
            for (lone, looks) in [(survivor, 2), (connecting, 2), (from_the_host, 1)] {
                // the snapshot's manifest names it
                let mut dumps = Dumps::of(&[&[lone]]);
                assert_eq!(manifest_sockets(&mut dumps).unwrap(), vec![lone]);
                assert_eq!(dumps.taken, looks);
                // and a restore whose manifest does not name it retires it
                let mut dumps = Dumps::of(&[&[lone]]);
                assert_eq!(unlisted_sockets(&mut dumps, &empty).unwrap(), vec![lone]);
                assert_eq!(dumps.taken, looks);
            }
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
        /// How many of the next removals of the whole gate fail.
        failing_gate_opens: u32,
        fail_barrier: bool,
        fail_dump: bool,
        fail_destroy: bool,
        fail_hook: bool,
        /// Calls to the model diagnostic's destroy, one datagram each.
        destroy_calls: usize,
        /// Sockets the guest's own daemons open once egress is open.
        opened_after_egress: Vec<TcpSocketIdentity>,
        /// Those sockets once they are open: made after the restore's list.
        made_after_the_list: Vec<TcpSocketIdentity>,
        /// Sockets a dump in progress misses and the next one shows.
        appear_on_the_next_dump: Vec<TcpSocketIdentity>,
        /// Whether the guest's listeners refuse a handshake from a SYN cookie.
        syn_cookies_refused: bool,
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
            let opened = std::mem::take(&mut state.opened_after_egress);
            state.live.extend(opened.iter().copied());
            state.made_after_the_list.extend(opened);
            Ok(())
        }

        async fn open(&self) -> Result<()> {
            let mut state = self.0.lock().unwrap();
            if state.failing_gate_opens > 0 {
                state.failing_gate_opens -= 1;
                state.events.push("gate-open-failed");
                bail!("injected gate removal failure");
            }
            state.gate_closed = false;
            state.egress_closed = false;
            state.udp_gated = false;
            state.syn_cookies_refused = false;
            state.events.push("gate-open");
            Ok(())
        }

        fn refuse_syn_cookies(&self) -> Result<()> {
            let mut state = self.0.lock().unwrap();
            assert!(
                !state.udp_gated,
                "SYN cookies were refused after the restore's UDP rejection: a restore \
                 refuses them before anything else"
            );
            state.syn_cookies_refused = true;
            state.events.push("syn-cookies-off");
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
            assert!(
                state.syn_cookies_refused,
                "the restore hook ran while a listener still completes a handshake from a SYN \
                 cookie: a client reaches the clone before it is published"
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
            // The UDP rejection marks a restore. Its list is the last look at what was
            // made behind the gate, so nothing may be made behind it afterwards.
            assert!(
                !state.udp_gated || state.syn_cookies_refused,
                "the restore listed the guest's sockets while a listener still completes a \
                 handshake from a SYN cookie: a socket made after the list is in neither"
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
            let listed = state.live.clone();
            let appear = std::mem::take(&mut state.appear_on_the_next_dump);
            state.live.extend(appear);
            Ok(listed)
        }

        fn loopback_client_ports(&self) -> Result<std::collections::HashSet<u16>> {
            Ok([LOOPBACK_PORT].into_iter().collect())
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
                assert!(
                    !state.made_after_the_list.contains(socket),
                    "restore retired a socket the guest opened after the restore's list: \
                     {}",
                    socket.describe()
                );
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
        /// The model kernel holds the manifest's sockets and no other.
        fn dump_tcp_sockets(&mut self) -> Result<Vec<TcpSocketIdentity>> {
            Ok(Vec::new())
        }

        fn loopback_client_ports(&self) -> Result<std::collections::HashSet<u16>> {
            Ok(Default::default())
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
                    "-p tcp -m tcp --tcp-flags SYN,ACK SYN -j REJECT --reject-with tcp-reset",
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

    /// The body of `fn <name>(` in `source`, up to its closing brace.
    fn function_body<'a>(source: &'a str, signature: &str) -> &'a str {
        let start = source
            .find(signature)
            .unwrap_or_else(|| panic!("{signature} is gone"));
        let len = source[start..]
            .find("\n}\n")
            .unwrap_or_else(|| panic!("{signature} has no closing brace"));
        &source[start..start + len]
    }

    /// A snapshot of a running VM is paused right after the prepare returns, so the
    /// prepare holds the agent's console quiet after its last step, and only a
    /// prepare that holds it succeeds (CI run 41036024614: a startup snapshot taken
    /// while the agent logged the gate's refusals restored with a dead console).
    #[test]
    fn the_prepare_holds_the_console_quiet_after_its_last_step() {
        let body = function_body(
            include_str!("snapshot_network.rs"),
            "pub async fn prepare_snapshot_network(",
        );
        let prepared = body
            .find("prepare_with(")
            .expect("the prepare no longer closes the gate");
        let held = body
            .find("crate::console::request_source_quiesce()")
            .expect("the prepare no longer holds the console quiet for the pause");
        assert!(prepared < held, "prepared={prepared} held={held}");
        assert!(
            body[held..]
                .contains(".context(\"holding the console quiet for the snapshot pause\")?"),
            "a console that cannot be held quiet must fail the prepare:\n{body}"
        );
    }

    /// The source's resume releases the hold before anything that can fail.
    #[test]
    fn the_source_resume_releases_the_console_first() {
        let body = function_body(
            include_str!("snapshot_network.rs"),
            "async fn resume_source_network_with<",
        );
        let released = body
            .find("crate::console::request_source_release()")
            .expect("the source's resume no longer releases the console");
        let reopened = body
            .find("reopen_owed_gate(gate, REOPEN_PAUSE)")
            .expect("the resume reopens the gate");
        assert!(
            released < reopened,
            "released={released} reopened={reopened}"
        );
    }

    /// A clone inherits the hold its snapshot was taken under and releases it before
    /// the restore sequence logs anything, and the agent serves the requests.
    #[test]
    fn a_restored_clone_releases_the_inherited_console_hold_first() {
        let body = function_body(include_str!("mmds.rs"), "async fn apply_restore_epoch(");
        let released = body
            .find("crate::console::release_source_snapshot_hold(metadata.agent_log)")
            .expect("the restore no longer releases the console hold it inherits");
        let begun = body
            .find("signals.restore_status.begin()")
            .expect("the restore begins its generation");
        assert!(released < begun, "released={released} begun={begun}");
        assert!(
            include_str!("agent.rs").contains("crate::console::spawn_source_gate_listener()"),
            "the agent no longer listens for the boundary's console requests"
        );
    }

    /// A restore tells the console writer it was acknowledged, after the host has the
    /// ACK and the output reconnect is requested (`RestoreStatus::succeed`). Nothing
    /// else does: without the call the restore's progress lines are never queued for
    /// the host, wait out their window and go to the serial console ten seconds late.
    #[test]
    fn a_restore_tells_the_console_it_was_acknowledged_after_it_succeeds() {
        let body = function_body(include_str!("mmds.rs"), "async fn apply_restore_epoch(");
        let acked = body
            .find("crate::vsock::notify_restore_complete(")
            .expect("the restore acknowledges to the host");
        let succeeded = body
            .find("signals.restore_status.succeed()")
            .expect("the restore publishes its readiness");
        let told = body
            .find("crate::console::restore_log_acknowledged();")
            .expect("the restore never tells the console writer it was acknowledged");
        assert!(
            acked < succeeded && succeeded < told,
            "acked={acked} succeeded={succeeded} told={told}"
        );
    }

    /// The source's gate rejected any poll chronyd made during the cookie dump,
    /// so the source asks chronyd to measure once its gate has reopened.
    #[test]
    fn the_source_asks_chronyd_to_measure_after_its_gate_reopens() {
        let source = include_str!("snapshot_network.rs");
        let start = source
            .find("async fn resume_source_network_with<")
            .expect("resume_source_network_with is in snapshot_network.rs");
        let len = source[start..]
            .find("\n}\n")
            .expect("resume_source_network has a closing brace");
        let body = &source[start..start + len];
        let reopened = body
            .find("source VM snapshot network reopened")
            .expect("the source no longer reports its reopened gate");
        let burst = body
            .find("chrony_burst().await")
            .expect("the source no longer asks chronyd to measure after its gate reopens");
        assert!(reopened < burst, "reopened={reopened} burst={burst}");
    }

    /// chronyd goes offline while the gate still rejects its polls, so it cannot
    /// have measured the pause's lag; the gate opens before the clock steps, so a
    /// workload timer the step expires does not reconnect into the gate's reset; and
    /// chronyd comes back online only after the step.
    #[test]
    fn the_source_steps_its_clock_between_the_reopen_and_chronyd_coming_back() {
        let source = include_str!("snapshot_network.rs");
        let start = source
            .find("async fn resume_source_network_with<")
            .expect("resume_source_network_with is in snapshot_network.rs");
        let len = source[start..]
            .find("\n}\n")
            .expect("resume_source_network has a closing brace");
        let body = &source[start..start + len];
        let offline = body
            .find("chronyc(&[\"offline\"])")
            .expect("the source no longer takes chronyd offline around the step");
        let reopen = body
            .find("reopen_owed_gate(gate, REOPEN_PAUSE)")
            .expect("the source no longer reopens its gate");
        let step = body
            .find("crate::bootplan::advance_system_clock(host_time, started.elapsed())")
            .expect("the source no longer steps its clock to the host's");
        // The last one: a resume refused for a restore, before the reopen, also sets
        // chronyd online again on its way out.
        let online = body
            .rfind("chronyc(&[\"online\"])")
            .expect("the source no longer brings chronyd back online");
        assert!(
            offline < reopen && reopen < step && step < online,
            "offline={offline} reopen={reopen} step={step} online={online}"
        );
    }

    #[test]
    fn the_resume_takes_the_host_time_after_its_flag() {
        let args = |list: &[&str]| list.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
        assert_eq!(
            resume_host_time(&args(&[
                "fc-agent",
                "--resume-snapshot-network",
                "--host-time-ns",
                "1731301800123456789",
            ])),
            Some("1731301800123456789")
        );
        assert_eq!(
            resume_host_time(&args(&["fc-agent", "--resume-snapshot-network"])),
            None
        );
        assert_eq!(
            resume_host_time(&args(&[
                "fc-agent",
                "--resume-snapshot-network",
                "--host-time-ns"
            ])),
            None
        );
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
            "iptables-restore< -A FCVM_SNAPSHOT_IN -p tcp -m tcp --tcp-flags SYN,ACK SYN -j REJECT"
        ));
        assert!(calls.contains(
            "iptables-restore< -A FCVM_SNAPSHOT_OUT -p tcp -m tcp --tcp-flags SYN,ACK SYN -j REJECT"
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

    /// What the tracker would say of a packet, for the rules that ask it.
    #[derive(Clone, Copy, PartialEq, Debug)]
    enum Tracked {
        /// The first packet the tracker sees of its flow.
        New,
        /// The tracker holds an entry for the tuple and has seen both sides.
        Established,
    }

    /// A packet as the gate's rules can see it.
    #[derive(Clone, Copy, Debug)]
    struct Packet {
        protocol: &'static str,
        /// TCP flags that are set, as iptables names them.
        flags: &'static [&'static str],
        loopback_peer: bool,
        on_lo: bool,
        destination_port: u16,
        tracked: Tracked,
        /// Whether the packet runs the way its flow began.
        original_direction: bool,
    }

    const SYN: Packet = Packet {
        protocol: "tcp",
        flags: &["SYN"],
        loopback_peer: false,
        on_lo: false,
        destination_port: 7070,
        tracked: Tracked::New,
        original_direction: true,
    };

    #[derive(PartialEq, Debug)]
    enum Verdict {
        Rejected(String),
        Passed,
    }

    /// Run `packet` through a gate chain's rules as iptables would: the first
    /// rule whose every match holds decides. Every token of every rule has to be
    /// one this model knows, so a rule added to the gate has to be taught here
    /// before these tests can pass.
    fn through(rules: &[Vec<String>], loopback_flag: &str, packet: Packet) -> Verdict {
        for rule in rules {
            let mut tokens = rule.iter().map(String::as_str);
            let mut matches = true;
            let mut target = None;
            let mut reject_with = String::new();
            while let Some(token) = tokens.next() {
                let mut value = || tokens.next().expect("a value for the option");
                match token {
                    "-p" => matches &= value() == packet.protocol,
                    "-s" | "-d" => {
                        assert_eq!(token, loopback_flag, "the chain's own end is its loopback");
                        value();
                        matches &= packet.loopback_peer;
                    }
                    "-i" => matches &= value() == "lo" && packet.on_lo,
                    "-m" => {
                        value();
                    }
                    "--tcp-flags" => {
                        let examined: Vec<&str> = value().split(',').collect();
                        let set: Vec<&str> = value().split(',').collect();
                        matches &= examined
                            .iter()
                            .all(|flag| packet.flags.contains(flag) == set.contains(flag));
                    }
                    "--dport" => matches &= value() == packet.destination_port.to_string(),
                    "--ctstate" => {
                        matches &= match value() {
                            "NEW" => packet.tracked == Tracked::New,
                            other => panic!("the model does not know --ctstate {other}"),
                        }
                    }
                    "--ctdir" => {
                        matches &= match value() {
                            "ORIGINAL" => packet.original_direction,
                            other => panic!("the model does not know --ctdir {other}"),
                        }
                    }
                    "-j" => target = Some(value().to_string()),
                    "--reject-with" => reject_with = value().to_string(),
                    other => panic!("the model does not know {other} in {rule:?}"),
                }
            }
            if !matches {
                continue;
            }
            match target.as_deref() {
                Some("RETURN") => return Verdict::Passed,
                Some("REJECT") => return Verdict::Rejected(reject_with),
                other => panic!("the model does not know the target {other:?}"),
            }
        }
        Verdict::Passed
    }

    /// Both chains of both families: (chain name, its loopback flag, its rules).
    fn gate_chains() -> Vec<(String, &'static str, Vec<Vec<String>>)> {
        let mut chains = Vec::new();
        for network in [IPV4_LOOPBACK, IPV6_LOOPBACK] {
            for (name, flag) in [("INPUT", "-s"), ("OUTPUT", "-d")] {
                chains.push((
                    format!("{name} of {network}"),
                    flag,
                    gate_chain_rules(flag, network),
                ));
            }
        }
        chains
    }

    /// #2106: a SYN begins a connection whatever the tracker remembers of its
    /// tuple. A guest keeps ESTABLISHED entries for connections whose end it never
    /// tracked, and a SYN on such a tuple is not NEW, so the rule that asked the
    /// tracker let it through the closed gate and a restored clone answered a
    /// client before publication.
    ///
    /// RED BEFORE THE FIX: with `--ctstate NEW` the tracked SYN passed all four
    /// chains.
    #[test]
    fn a_syn_is_refused_whatever_the_tracker_holds_for_its_tuple() {
        let reset = Verdict::Rejected("tcp-reset".to_string());
        for (chain, flag, rules) in gate_chains() {
            for tracked in [Tracked::New, Tracked::Established] {
                let syn = Packet { tracked, ..SYN };
                assert_eq!(through(&rules, flag, syn), reset, "{chain}, {tracked:?}");
                // garbage a listener drops is refused as well, not passed to it
                for flags in [&["SYN", "FIN"][..], &["SYN", "RST"], &["SYN", "PSH", "URG"]] {
                    let odd = Packet { flags, ..syn };
                    assert_eq!(through(&rules, flag, odd), reset, "{chain}, {flags:?}");
                }
            }
        }
    }

    /// A segment of a connection that is already open is not a new flow, and the
    /// gate passes it whether or not the tracker has ever seen the connection.
    /// With tracking attached only while the gate was closed, the first segment
    /// it saw of a live connection was NEW, and the snapshot reset a connection
    /// of the VM it was taken from.
    ///
    /// RED BEFORE THE FIX: every untracked segment below was rejected.
    #[test]
    fn a_segment_of_an_open_connection_passes_untracked() {
        for (chain, flag, rules) in gate_chains() {
            for flags in [
                &["ACK"][..],
                &["ACK", "PSH"],
                &["ACK", "FIN"],
                &["RST"],
                &["SYN", "ACK"],
            ] {
                let segment = Packet {
                    flags,
                    tracked: Tracked::New,
                    ..SYN
                };
                assert_eq!(
                    through(&rules, flag, segment),
                    Verdict::Passed,
                    "{chain}, {flags:?}"
                );
            }
        }
    }

    /// The guest talking to itself is not gated: a SYN to or from loopback
    /// passes both chains, and one entering on `lo` passes INPUT.
    #[test]
    fn loopback_connections_pass_the_closed_gate() {
        for (chain, flag, rules) in gate_chains() {
            let own = Packet {
                loopback_peer: true,
                ..SYN
            };
            assert_eq!(through(&rules, flag, own), Verdict::Passed, "{chain}");
            if flag == "-s" {
                let redirected = Packet { on_lo: true, ..SYN };
                assert_eq!(
                    through(&rules, flag, redirected),
                    Verdict::Passed,
                    "{chain}"
                );
            }
        }
    }

    /// Closing the gate must not attach connection tracking: no rule of the
    /// chains a snapshot installs names it. The restored clone's UDP rule is
    /// the one rule that does, and it is not among them.
    ///
    /// RED BEFORE THE FIX: the TCP rule of every chain was `-m conntrack`.
    #[test]
    fn no_rule_the_snapshot_side_installs_uses_connection_tracking() {
        for network in [IPV4_LOOPBACK, IPV6_LOOPBACK] {
            let input = gate_chains_restore_input(network);
            assert!(
                !input.contains("conntrack") && !input.contains("--ct"),
                "closing the gate attaches connection tracking:\n{input}"
            );
            assert!(
                udp_ingress_rule_args(network).contains(&"conntrack"),
                "the model below assumes the UDP rule is the tracked one"
            );
        }
    }

    /// What the restored clone's UDP rule refuses and what it passes, with the
    /// rule in its place in the INPUT chain. A datagram from outside is refused
    /// whether its flow is new or one a client began before the snapshot; a
    /// datagram on a flow the guest began passes, which is what a reply is.
    #[test]
    fn the_udp_rule_refuses_by_direction_and_passes_replies() {
        for network in [IPV4_LOOPBACK, IPV6_LOOPBACK] {
            let mut rules = gate_chain_rules("-s", network);
            rules.insert(
                2,
                udp_ingress_rule_args(network).map(str::to_string).to_vec(),
            );
            let datagram = Packet {
                protocol: "udp",
                flags: &[],
                ..SYN
            };
            let unreachable = Verdict::Rejected(port_unreachable(network).to_string());
            for tracked in [Tracked::New, Tracked::Established] {
                let from_a_client = Packet {
                    tracked,
                    ..datagram
                };
                assert_eq!(through(&rules, "-s", from_a_client), unreachable);
            }
            let reply = Packet {
                tracked: Tracked::Established,
                original_direction: false,
                ..datagram
            };
            assert_eq!(through(&rules, "-s", reply), Verdict::Passed);
            let own = Packet {
                on_lo: true,
                ..datagram
            };
            assert_eq!(through(&rules, "-s", own), Verdict::Passed);
        }
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
        -A FCVM_SNAPSHOT_IN -p tcp -m tcp --tcp-flags SYN,ACK SYN -j REJECT --reject-with tcp-reset\n\
        -A FCVM_SNAPSHOT_IN -j RETURN\n\
        -A FCVM_SNAPSHOT_OUT -d 127.0.0.0/8 -p tcp -j RETURN\n\
        -A FCVM_SNAPSHOT_OUT -p udp -m udp --dport 123 -j REJECT --reject-with icmp-port-unreachable\n\
        -A FCVM_SNAPSHOT_OUT -p tcp -m tcp --tcp-flags SYN,ACK SYN -j REJECT --reject-with tcp-reset\n\
        -A FCVM_SNAPSHOT_OUT -j RETURN\n";

    /// Same capture for the IPv6 family (`-d ::1/128 -p tcp -j RETURN` etc.).
    /// The NTP lines of both dumps are iptables 1.8.10's rendering, legacy and
    /// nf_tables alike, of the rule as `gate_chain_rules` appends it (with
    /// `-m udp`), and the TCP reject lines are its nf_tables rendering of
    /// `TCP_NEW_FLOW_REJECT`, read back in a guest in both families; the rest of
    /// each dump is the capture described above.
    const ARMED_V6_DUMP: &str = "-P INPUT ACCEPT\n\
        -P FORWARD ACCEPT\n\
        -P OUTPUT ACCEPT\n\
        -N FCVM_SNAPSHOT_IN\n\
        -N FCVM_SNAPSHOT_OUT\n\
        -A INPUT -j FCVM_SNAPSHOT_IN\n\
        -A OUTPUT -j FCVM_SNAPSHOT_OUT\n\
        -A FCVM_SNAPSHOT_IN -s ::1/128 -p tcp -j RETURN\n\
        -A FCVM_SNAPSHOT_IN -i lo -j RETURN\n\
        -A FCVM_SNAPSHOT_IN -p tcp -m tcp --tcp-flags SYN,ACK SYN -j REJECT --reject-with tcp-reset\n\
        -A FCVM_SNAPSHOT_IN -j RETURN\n\
        -A FCVM_SNAPSHOT_OUT -d ::1/128 -p tcp -j RETURN\n\
        -A FCVM_SNAPSHOT_OUT -p udp -m udp --dport 123 -j REJECT --reject-with icmp6-port-unreachable\n\
        -A FCVM_SNAPSHOT_OUT -p tcp -m tcp --tcp-flags SYN,ACK SYN -j REJECT --reject-with tcp-reset\n\
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

    /// The step before egress (a routed clone's move to its own IPv6 address)
    /// runs with the clone's UDP rule in and egress still closed (#1114). The new
    /// address is what lets the network reach a routed clone, so before the UDP
    /// rule a datagram to it reached the restored workload; after egress opened,
    /// a connection could leave from the snapshot's address.
    #[tokio::test]
    async fn the_step_before_egress_runs_with_udp_gated_and_egress_closed() {
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
                sockets: Vec::new(),
            })
            .unwrap();
        let gate = ModelGate(state.clone());
        let barrier = ModelBarrier(state.clone());
        let mut diag = ModelDiag(state.clone());
        let seen = state.clone();
        restore_boundary_with(
            &gate,
            &barrier,
            &mut diag,
            &store,
            CLEANUP_BUDGET,
            || async move {
                let mut state = seen.lock().unwrap();
                assert!(
                    state.udp_gated,
                    "the clone's address came before its UDP rule"
                );
                assert!(
                    state.egress_closed,
                    "the clone's address came after egress opened"
                );
                state.events.push("ipv6-swap");
                Ok(())
            },
        )
        .await
        .unwrap();
        let events = state.lock().unwrap().events.clone();
        let at = |event: &str| events.iter().position(|e| *e == event).unwrap();
        assert!(
            at("gate-udp-ingress") < at("ipv6-swap") && at("ipv6-swap") < at("gate-open-egress"),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn cookie_bound_restore_never_kills_tuple_aba_replacement() {
        let stale = socket(21, [198, 51, 100, 8]);
        let replacement = socket(22, [198, 51, 100, 8]);
        let state = Arc::new(Mutex::new(ModelState {
            gate_closed: true,
            egress_closed: true,
            opened_after_egress: vec![replacement],
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
        // retired, and a daemon opens its tuple-identical replacement as soon
        // as egress is open, before the first socket is retired. A cookie-bound
        // destroy must therefore report already_gone and leave the replacement
        // alone; a tuple-based one would report a destroy and take the wrong
        // socket with it.
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
                unlisted: 0,
                datagrams: 1,
            }
        );
        assert!(report.verified_armed);
        let state = state.lock().unwrap();
        assert_eq!(state.live, vec![replacement]);
        assert_eq!(
            state.events,
            vec![
                "syn-cookies-off",
                "gate-verify",
                "gate-udp-ingress",
                "rx-barrier",
                "cookie-dump",
                "gate-open-egress",
                "cookie-destroy",
                "gate-open",
            ]
        );
        assert!(!state.gate_closed);
        assert!(!state.link_down);
    }

    /// #2123: a socket made behind the closed gate after the snapshot's dump is in the
    /// image and in no manifest. A listener that answered a SYN with a cookie takes the
    /// client's bare ACK, which the gate passes, for the whole handshake. The restore lists
    /// the guest's sockets itself before egress opens and retires what the manifest does
    /// not name with what it does, a `/loopback` port's client included, whose socket
    /// records a loopback peer. Connections with both ends in the guest are kept (a
    /// loopback one, and one over the guest's own address), and so is a connection the
    /// clone opens once egress is open.
    ///
    /// RED WITHOUT THE RESTORE'S OWN LIST: the cookie-born socket is live after the restore.
    /// RED WITH THE ADDRESS RULE: the `/loopback` port's is.
    #[tokio::test]
    async fn a_socket_made_behind_the_closed_gate_is_retired_with_the_manifests() {
        let listed = socket(31, [198, 51, 100, 8]);
        let cookie_born = socket(32, [198, 51, 100, 9]);
        let cookie_born_on_a_loopback_port = loopback_client_port_socket(38);
        let [loop_client, loop_server] = connection(33, 35, "127.0.0.1", "127.0.0.1");
        let [client, server] = in_guest_connection(36, 37);
        let reconnect = socket(34, [198, 51, 100, 8]);
        let kept = vec![loop_client, loop_server, client, server];
        let state = Arc::new(Mutex::new(ModelState {
            gate_closed: true,
            egress_closed: true,
            live: [
                vec![listed, cookie_born, cookie_born_on_a_loopback_port],
                kept.clone(),
            ]
            .concat(),
            opened_after_egress: vec![reconnect],
            ..Default::default()
        }));
        let store = MemoryStore::default();
        store
            .save(&SnapshotNetworkManifest {
                version: MANIFEST_VERSION,
                routes: Vec::new(),
                sockets: vec![listed],
            })
            .unwrap();
        let mut diag = ModelDiag(state.clone());

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
                destroyed: 1,
                already_gone: 0,
                unlisted: 2,
                datagrams: 1,
            }
        );
        let state = state.lock().unwrap();
        assert_eq!(state.live, [kept, vec![reconnect]].concat());
        // a socket the manifest does not name has no mirror image, so the list is taken
        // twice before it is given up
        assert_eq!(
            state.events,
            vec![
                "syn-cookies-off",
                "gate-verify",
                "gate-udp-ingress",
                "rx-barrier",
                "cookie-dump",
                "cookie-dump",
                "gate-open-egress",
                "cookie-destroy",
                "cookie-destroy",
                "cookie-destroy",
                "gate-open",
            ]
        );
        assert!(!state.syn_cookies_refused, "publication puts cookies back");
    }

    /// The manifest a snapshot stores names a `/loopback` port's client and a connection
    /// over the guest's own address, and no loopback connection inside the guest.
    ///
    /// RED WITH THE ADDRESS RULE: the manifest left the `/loopback` client's socket out
    /// for its 127.0.0.1 peer, and every clone of the source kept that connection.
    #[tokio::test]
    async fn the_manifest_names_a_loopback_ports_client_and_no_loopback_connection() {
        let outside = socket(61, [198, 51, 100, 8]);
        let from_the_host = loopback_client_port_socket(62);
        let [loop_client, loop_server] = connection(63, 64, "127.0.0.1", "127.0.0.1");
        let [own_client, own_server] = in_guest_connection(65, 66);
        let state = Arc::new(Mutex::new(ModelState {
            live: vec![
                outside,
                from_the_host,
                loop_client,
                loop_server,
                own_client,
                own_server,
            ],
            ..Default::default()
        }));
        let store = MemoryStore::default();
        let mut diag = ModelDiag(state.clone());
        prepare_with(
            &ModelGate(state.clone()),
            &ModelBarrier(state.clone()),
            &mut diag,
            &store,
        )
        .await
        .unwrap();
        assert_eq!(
            store.load().unwrap().sockets,
            vec![outside, from_the_host, own_client, own_server]
        );
    }

    /// A loopback connection made inside the guest while the restore lists its sockets:
    /// the list's first dump shows the server's end alone, and the restore looks again
    /// before it retires anything.
    ///
    /// RED WITH ONE DUMP: the server's end is retired.
    #[tokio::test]
    async fn a_connection_made_inside_the_guest_during_the_list_is_kept() {
        let [client, server] = connection(71, 72, "127.0.0.1", "127.0.0.1");
        let server = as_dual_stack(server);
        let state = Arc::new(Mutex::new(ModelState {
            gate_closed: true,
            egress_closed: true,
            live: vec![server],
            appear_on_the_next_dump: vec![client],
            ..Default::default()
        }));
        let store = MemoryStore::default();
        store
            .save(&SnapshotNetworkManifest {
                version: MANIFEST_VERSION,
                routes: Vec::new(),
                sockets: Vec::new(),
            })
            .unwrap();
        let mut diag = ModelDiag(state.clone());
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
        assert_eq!(report.tally.unlisted, 0);
        assert_eq!(state.lock().unwrap().live, vec![server, client]);
    }

    /// A four-tuple is not a socket identity, before egress opens as after it. The one
    /// connection that can reuse a retired socket's tuple behind the closed gate has both
    /// ends in the guest, on a loopback address, which both chains of the gate pass. The
    /// manifest names what was left of an earlier connection on that tuple when the
    /// snapshot was taken (its server's end, the client's already closed and freed), and
    /// that socket is gone; the new pair has the same tuples and is kept. (With a peer
    /// outside the guest the same thing is the cookie-born socket of the test above, and
    /// is retired.)
    #[tokio::test]
    async fn a_tuple_reused_inside_the_guest_behind_the_gate_is_kept() {
        let gone = connection(41, 42, "127.0.0.1", "127.0.0.1");
        let reused = connection(43, 44, "127.0.0.1", "127.0.0.1");
        let state = Arc::new(Mutex::new(ModelState {
            gate_closed: true,
            egress_closed: true,
            live: reused.to_vec(),
            ..Default::default()
        }));
        let store = MemoryStore::default();
        store
            .save(&SnapshotNetworkManifest {
                version: MANIFEST_VERSION,
                routes: Vec::new(),
                sockets: vec![gone[1]],
            })
            .unwrap();
        let mut diag = ModelDiag(state.clone());

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
                destroyed: 0,
                already_gone: 1,
                unlisted: 0,
                datagrams: 1,
            }
        );
        assert_eq!(state.lock().unwrap().live, reused.to_vec());
    }

    /// What the restore retires beyond the manifest: a socket the manifest names is not
    /// listed twice, and a connection inside the guest is in neither, also when a
    /// dual-stack server holds its end as an IPv6 socket with IPv4-mapped addresses. A
    /// connect the egress proxy took behind the closed egress half has two sockets that
    /// are not each other's mirror image, and both are retired.
    #[test]
    fn the_unlisted_sockets_are_those_leaving_the_guest_that_the_manifest_does_not_name() {
        let [client, server] = in_guest_connection(51, 52);
        let listed = socket(53, [198, 51, 100, 8]);
        let unlisted = socket(54, [198, 51, 100, 9]);
        let asked: (IpAddr, u16) = ("203.0.113.7".parse().unwrap(), 443);
        let from: (IpAddr, u16) = ("10.0.2.100".parse().unwrap(), 40001);
        let proxy: (IpAddr, u16) = (IpAddr::V4(Ipv4Addr::LOCALHOST), 12345);
        let redirected = [end_at(55, from, asked), end_at(56, proxy, from)];
        let manifest = SnapshotNetworkManifest {
            version: MANIFEST_VERSION,
            routes: Vec::new(),
            sockets: vec![listed],
        };
        for server in [server, as_dual_stack(server)] {
            let mut dumps = Dumps::of(&[&[client, server, listed, unlisted]]);
            assert_eq!(
                unlisted_sockets(&mut dumps, &manifest).unwrap(),
                vec![unlisted]
            );
        }
        let mut dumps = Dumps::of(&[&[client, server, listed, redirected[0], redirected[1]]]);
        assert_eq!(
            unlisted_sockets(&mut dumps, &manifest).unwrap(),
            redirected.to_vec()
        );
    }

    /// A publication whose gate removal fails leaves the clone unpublished, and an
    /// unpublished clone takes no SYN cookie: the setting is still 0, and the value to
    /// put back is still held for the publication that succeeds.
    ///
    /// RED WITH THE SETTING WRITTEN BACK FIRST: it reads 1 behind a gate that is closed.
    #[tokio::test]
    async fn a_failed_publication_leaves_syn_cookies_off() {
        struct NoNetfilter;
        impl NetfilterOpener for NoNetfilter {
            type Channel = crate::nftables::model::Channel;
            fn take(&self) -> Result<Self::Channel> {
                bail!("injected: no nfnetlink socket")
            }
            fn give_back(&self, _channel: Self::Channel) {}
        }
        let dir = tempfile::tempdir().unwrap();
        let setting = dir.path().join("tcp_syncookies");
        std::fs::write(&setting, "1\n").unwrap();
        let gate = IptablesGate::with_netfilter(RecordingRunner::default(), NoNetfilter)
            .with_syn_cookies(&setting);
        gate.refuse_syn_cookies().unwrap();
        gate.open()
            .await
            .expect_err("the gate's removal has no socket to go over");
        assert_eq!(std::fs::read_to_string(&setting).unwrap(), "0");
        assert_eq!(
            gate.syn_cookies_before.lock().unwrap().as_deref(),
            Some("1")
        );
    }

    /// The clone stops taking SYN cookies before it reads anything, and gets its setting
    /// back when it is published, whatever the setting was. A kernel without the setting
    /// sends no cookies, and a gate that was given no setting touches none.
    #[test]
    fn the_clone_refuses_syn_cookies_until_it_is_published() {
        let dir = tempfile::tempdir().unwrap();
        let setting = dir.path().join("tcp_syncookies");
        for before in ["1\n", "2\n"] {
            std::fs::write(&setting, before).unwrap();
            let gate = IptablesGate::new(RecordingRunner::default()).with_syn_cookies(&setting);
            gate.refuse_syn_cookies().unwrap();
            assert_eq!(std::fs::read_to_string(&setting).unwrap(), "0");
            gate.restore_syn_cookies();
            assert_eq!(std::fs::read_to_string(&setting).unwrap(), before.trim());
            // once: a second publication writes nothing
            std::fs::write(&setting, "7").unwrap();
            gate.restore_syn_cookies();
            assert_eq!(std::fs::read_to_string(&setting).unwrap(), "7");
        }
        // already off: nothing to put back
        std::fs::write(&setting, "0\n").unwrap();
        let gate = IptablesGate::new(RecordingRunner::default()).with_syn_cookies(&setting);
        gate.refuse_syn_cookies().unwrap();
        std::fs::write(&setting, "1").unwrap();
        gate.restore_syn_cookies();
        assert_eq!(std::fs::read_to_string(&setting).unwrap(), "1");
        // no such setting, and no setting given
        std::fs::remove_file(&setting).unwrap();
        gate.refuse_syn_cookies().unwrap();
        gate.restore_syn_cookies();
        assert!(!setting.exists());
        let plain = IptablesGate::new(RecordingRunner::default());
        plain.refuse_syn_cookies().unwrap();
        plain.restore_syn_cookies();
        // a value that cannot be written back does not stop publication
        std::fs::write(&setting, "1").unwrap();
        let gate = IptablesGate::new(RecordingRunner::default()).with_syn_cookies(&setting);
        gate.refuse_syn_cookies().unwrap();
        std::fs::remove_file(&setting).unwrap();
        std::fs::create_dir(&setting).unwrap();
        gate.restore_syn_cookies();
        assert!(gate.syn_cookies_before.lock().unwrap().is_none());

        // the restore's first step, and publication's first
        let source = include_str!("snapshot_network.rs");
        let restore = function_body(source, "async fn restore_boundary_with<");
        assert!(
            restore.find("gate.refuse_syn_cookies()").unwrap()
                < restore.find("gate.is_closed()").unwrap()
        );
        let open = function_body(
            source,
            "    async fn open(&self) -> Result<()> {\n        // The setting comes back",
        );
        assert!(
            open.find("self.remove_halves(").unwrap()
                < open.find("self.restore_syn_cookies();").unwrap()
        );
        // only the restore: a snapshot does not reconfigure its source
        assert!(!function_body(source, "async fn prepare_with<").contains("syn_cookies"));
        assert!(
            !function_body(source, "pub async fn prepare_snapshot_network(")
                .contains("syn_cookies")
        );
        // A snapshot of this clone waits for its publication: the prepare takes the lock
        // the restore holds, with the gate that knows the value, until `publish` ends,
        // before it does anything to the guest.
        let prepare = function_body(source, "async fn prepare_snapshot_network_in<");
        assert!(!prepare.contains("syn_cookies"));
        let locked = prepare
            .find("let _transaction_lock = acquire_boundary_lock_at(lock)")
            .expect("the prepare takes no boundary lock");
        assert!(locked < prepare.find(".mark_owed(token)").unwrap());
        assert!(locked < prepare.find("freeze();").unwrap());
        assert!(locked < prepare.find("boundary()").unwrap());
        assert!(
            function_body(source, "pub async fn prepare_snapshot_network(")
                .contains("Path::new(BOUNDARY_LOCK_PATH),")
        );
        let restored = function_body(source, "pub struct RestoredNetwork {");
        assert!(restored.contains("_transaction_lock: std::fs::File,"));
        assert!(restored.contains("gate: IptablesGate<SystemCommandRunner>,"));
        // A clone whose restore or publication failed keeps the 0 and is powered off:
        // both steps are inside `handle_clone_restore`, and its failure shuts the VM down.
        let restore = function_body(
            include_str!("restore.rs"),
            "pub async fn handle_clone_restore(",
        );
        assert!(restore.contains("crate::snapshot_network::restore_snapshot_network("));
        assert!(restore.contains(".publish(restore_hook,"));
        let mmds = include_str!("mmds.rs");
        let failed = &mmds[mmds
            .find("Output/exec readiness will not be published; shutting down clone")
            .unwrap()..];
        assert!(failed[..200].contains("crate::system::shutdown_vm(1).await"));
        // One network namespace holds the gate, the socket list, the setting and every
        // listener of the workload.
        assert!(include_str!("container.rs").contains("args.push(\"--network=host\".to_string());"));
    }

    /// A restore that cannot list the guest's sockets cannot say none was made behind the
    /// gate, so it stays unpublished, with egress still closed and no socket retired.
    #[tokio::test]
    async fn a_failed_listing_keeps_the_restored_clone_closed() {
        let listed = socket(35, [198, 51, 100, 8]);
        let state = Arc::new(Mutex::new(ModelState {
            gate_closed: true,
            egress_closed: true,
            live: vec![listed],
            fail_dump: true,
            ..Default::default()
        }));
        let store = MemoryStore::default();
        store
            .save(&SnapshotNetworkManifest {
                version: MANIFEST_VERSION,
                routes: Vec::new(),
                sockets: vec![listed],
            })
            .unwrap();
        let mut diag = ModelDiag(state.clone());

        restore_with(
            &ModelGate(state.clone()),
            &ModelBarrier(state.clone()),
            &mut diag,
            &store,
            &NoRestoreHook,
            CLEANUP_BUDGET,
        )
        .await
        .expect_err("a restore that cannot list sockets must not publish");
        let state = state.lock().unwrap();
        assert!(state.gate_closed && state.egress_closed);
        assert_eq!(state.live, vec![listed]);
        assert_eq!(
            state.events,
            vec![
                "syn-cookies-off",
                "gate-verify",
                "gate-udp-ingress",
                "rx-barrier",
                "cookie-dump"
            ]
        );
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
                "syn-cookies-off",
                "gate-verify",
                "gate-close",
                "gate-udp-ingress",
                "rx-barrier",
                "cookie-dump",
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
                "syn-cookies-off",
                "gate-verify",
                "gate-udp-ingress",
                "rx-barrier",
                "cookie-dump",
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
                "syn-cookies-off",
                "gate-verify",
                "gate-close",
                "gate-udp-ingress",
                "rx-barrier",
                "cookie-dump",
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
                "syn-cookies-off",
                "gate-verify",
                "gate-udp-ingress",
                "rx-barrier",
                "cookie-dump",
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
                "syn-cookies-off",
                "gate-verify",
                "gate-udp-ingress",
                "rx-barrier",
                "cookie-dump",
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
        let text = format!("{error:#}");
        // The error says it hit the budget, how long the reap after the kill took,
        // and what the guest looked like, which is what tells a starved restore
        // from a slow one.
        assert!(text.contains("did not finish within 300ms"), "{text}");
        assert!(text.contains("killed and reaped elapsed_ms="), "{text}");
        assert!(
            text.contains("MemAvailable") && text.contains("procs_blocked="),
            "the error carries no vitals: {text}"
        );
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

    /// A killed child that is not reaped, or whose wait fails, is named by its pid,
    /// and the restore fails closed rather than waiting on it.
    #[test]
    fn a_timeout_names_a_child_that_was_not_reaped() {
        let budget = std::time::Duration::from_millis(300);
        assert_eq!(
            timeout_message(budget, Some(1234), Reap::NotReaped, "v"),
            "did not finish within 300ms; pid 1234 was not reaped 5s after SIGKILL; v"
        );
        let failed = Reap::WaitFailed(std::io::Error::from_raw_os_error(libc::ECHILD));
        let text = timeout_message(budget, None, failed, "v");
        assert!(
            text.starts_with(
                "did not finish within 300ms; killed pid ?, and waiting for it failed: "
            ),
            "{text}"
        );
    }

    /// A command is over when its process exits. One that leaves a process
    /// running with its pipes (a hook that starts a daemon) returns then, with
    /// what it wrote itself; what the process it left writes later is not waited
    /// for.
    ///
    /// RED BEFORE THE FIX: the run waited for the end of the output, which the
    /// process left behind holds off for as long as it lives, so the run ended at
    /// its budget and failed.
    #[tokio::test]
    async fn a_command_that_leaves_a_process_running_ends_at_its_own_exit() {
        let argv: Vec<String> = [
            "sh",
            "-c",
            "sleep 60 & echo left $!; (sleep 0.3; echo late) & echo started",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        let output = run_bounded(&argv, std::time::Duration::from_secs(20)).await;
        let output = output.expect("the run waited for the process the command left behind");
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        // The sleeper is this test's to end, whatever the assertions say.
        let left: Option<i32> = stdout
            .lines()
            .find_map(|line| line.strip_prefix("left ")?.trim().parse().ok());
        let alive = left.is_some_and(|pid| unsafe { libc::kill(pid, 0) } == 0);
        if let Some(pid) = left {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        assert!(output.status.success(), "{:?}", output.status);
        assert!(
            stdout.contains("started") && !stdout.contains("late"),
            "{stdout:?}"
        );
        assert!(
            alive,
            "the process the command left running was not left running"
        );
    }

    /// Read as the container's user, a file is absent only when the reader says
    /// so. A reader that cannot run fails the read, even when its error says "No
    /// such file or directory", as a prefix whose program is missing does.
    ///
    /// RED BEFORE THE FIX: any stderr holding "No such file or directory" read as
    /// an absent file, which skipped the hook.
    #[tokio::test]
    async fn a_reader_that_cannot_run_is_not_an_absent_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("id");
        std::fs::write(&file, "abc\n").unwrap();
        let prefix =
            |words: &[&str]| -> Vec<String> { words.iter().map(|w| w.to_string()).collect() };
        let env = prefix(&["env"]);
        assert_eq!(
            read_file_as(&env, &file, 16).await.unwrap().as_deref(),
            Some(&b"abc\n"[..])
        );
        assert_eq!(
            read_file_as(&env, &dir.path().join("absent"), 16)
                .await
                .unwrap(),
            None
        );
        assert!(
            read_file_as(&env, &file, 3).await.is_err(),
            "over the limit"
        );
        // With room for the reader's own error, as the id file's read has.
        let missing = prefix(&["env", "fcvm-test-no-such-program"]);
        let error = read_file_as(&missing, &file, 256)
            .await
            .expect_err("a reader that could not run read as an absent file");
        assert!(format!("{error:#}").contains("exited"), "{error:#}");
        assert!(
            read_file_as(&env, dir.path(), 16).await.is_err(),
            "a directory was read"
        );
        // As root, directly: the same three answers.
        assert_eq!(
            read_file_as(&[], &file, 16).await.unwrap().as_deref(),
            Some(&b"abc\n"[..])
        );
        assert_eq!(
            read_file_as(&[], &dir.path().join("absent"), 16)
                .await
                .unwrap(),
            None
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
        let publish = at(".publish(restore_hook,");
        assert!(
            nfs < publish,
            "the hook runs before the NFS shares are remounted: a hook reading a share \
             blocks on the dead hard mount"
        );
        assert!(
            waits < publish,
            "the hook runs before the egress proxy and exec server are back"
        );
        // And the fresh chronyd does not start beside the hook, whose vCPU it
        // would share: it starts after the publication.
        assert!(
            publish < at("fresh_chronyd.start();"),
            "the fresh chronyd starts before the hook has run"
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

    /// The restoring namespace's MTU is on eth0 before the boundary opens egress:
    /// every daemon that reconnects then opens its connection with the MSS that
    /// MTU allows. Set after, the reconnects that bring a restored web-server clone about
    /// 1 GB from host services in its first 30 s arrive in 1514-byte frames, and
    /// the clone's TAP dropped 75 to 86 percent of them (2026-11-16).
    #[test]
    fn the_guest_mtu_is_set_before_the_boundary_opens_egress() {
        let body = crate::restore::handle_clone_restore_source();
        let at = |needle: &str| {
            body.find(needle)
                .unwrap_or_else(|| panic!("{needle} is not in handle_clone_restore"))
        };
        assert!(
            at("network::apply_guest_mtu(mtu)")
                < at("crate::snapshot_network::restore_snapshot_network("),
            "the guest MTU is set after the boundary opens egress"
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
    /// publication, asked of the container's runtime, not a marker set when the
    /// agent was about to start it. A user snapshot taken between that marker and
    /// podman's start recorded a container that did not exist, and its clones
    /// failed their restore on `podman exec`; the opposite order would have
    /// skipped the hook for a container that did exist.
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
        let id = "ab".repeat(32);
        for (runtime_dir, root) in [
            (None, "/run/crun"),
            (Some("/run/user/1000"), "/run/user/1000/crun"),
        ] {
            let runtime = crate::container::ContainerRuntime::new(runtime_dir, None);
            let argv = ContainerRestoreHook::state_argv(&runtime, &id);
            assert_eq!(
                argv,
                ["crun", "--root", root, "state", id.as_str()],
                "the hook does not read the container's state from its runtime"
            );
        }
        // The decision is made from that read: without `running` from the runtime,
        // podman's report that the container started runs no hook.
        use std::os::unix::process::ExitStatusExt;
        let created = BoundedOutput {
            status: std::process::ExitStatus::from_raw(0),
            stdout: br#"{"status": "created"}"#.to_vec(),
            stderr: Vec::new(),
        };
        assert!(matches!(
            hook_eligibility(&created, true),
            HookEligibility::Skip(_)
        ));
    }

    /// What the hook asked of its runtime, and what the runtime answers.
    struct FakeRuntime {
        state: Option<HookEligibility>,
        exec: std::result::Result<(i32, &'static str, String), &'static str>,
        asked: Mutex<Vec<&'static str>>,
    }

    impl HookRuntime for FakeRuntime {
        async fn state(&self) -> Result<HookEligibility> {
            self.asked.lock().unwrap().push("state");
            Ok(match self.state.as_ref().expect("the state was read") {
                HookEligibility::Run => HookEligibility::Run,
                HookEligibility::Skip(reason) => HookEligibility::Skip(reason.clone()),
                HookEligibility::Fail(reason) => HookEligibility::Fail(reason.clone()),
            })
        }

        async fn exec(&self) -> Result<BoundedOutput> {
            self.asked.lock().unwrap().push("exec");
            match &self.exec {
                Ok((code, stdout, stderr)) => Ok(BoundedOutput {
                    status: std::process::ExitStatus::from_raw(code << 8),
                    stdout: stdout.as_bytes().to_vec(),
                    stderr: stderr.as_bytes().to_vec(),
                }),
                Err(error) => bail!("{error}"),
            }
        }
    }

    /// What `run_hook` asks of the runtime and what it makes of the answers.
    ///
    /// With podman's report that the container runs, the command is run and
    /// nothing else is asked, whether it succeeds or fails. A hook that ran and
    /// failed is returned as its failure, whatever the container does after;
    /// only crun's own refusal to enter a container that is stopped or paused
    /// skips the hook, and its refusal for want of any such container fails the
    /// restore. Without podman's report the state is read first.
    ///
    /// RED BEFORE THE FIX: every hook was preceded by a `crun state`; then, with
    /// the command run first, a hook that failed was followed by a state read
    /// whose answer could skip it, so a failed hook in a container that had since
    /// stopped published the clone.
    #[tokio::test]
    async fn the_hook_asks_its_runtime_only_what_it_does_not_know() {
        let id = "ab".repeat(32);
        let run =
            |reported_running: bool,
             state: Option<HookEligibility>,
             exec: std::result::Result<(i32, &'static str, String), &'static str>| {
                let id = id.clone();
                async move {
                    let runtime = FakeRuntime {
                        state,
                        exec,
                        asked: Mutex::new(Vec::new()),
                    };
                    let outcome = run_hook(&runtime, &id, reported_running)
                        .await
                        .map(|output| output.map(|output| output.status.code()))
                        .map_err(|error| format!("{error:#}"));
                    (runtime.asked.into_inner().unwrap(), outcome)
                }
            };
        let none = String::new;

        // podman reported the container running: the command alone.
        assert_eq!(
            run(true, None, Ok((0, "done", none()))).await,
            (vec!["exec"], Ok(Some(Some(0))))
        );
        // A hook that ran and failed is its failure: no state read can skip it.
        assert_eq!(
            run(true, None, Ok((7, "out", "err".to_string()))).await,
            (vec!["exec"], Ok(Some(Some(7))))
        );
        // So is one whose own message resembles crun's, and one that exits 1.
        assert_eq!(
            run(
                true,
                None,
                Ok((1, "", "the service is not running".to_string()))
            )
            .await,
            (vec!["exec"], Ok(Some(Some(1))))
        );
        assert_eq!(
            run(
                true,
                None,
                Ok((
                    1,
                    "restarting",
                    format!("the container `{id}` is not running")
                ))
            )
            .await,
            (vec!["exec"], Ok(Some(Some(1))))
        );
        // crun's refusals, as crun 1.14.1 prints them.
        for refusal in ["is not running", "is paused"] {
            assert_eq!(
                run(
                    true,
                    None,
                    Ok((1, "", format!("the container `{id}` {refusal}\n")))
                )
                .await,
                (vec!["exec"], Ok(None)),
                "{refusal}"
            );
        }
        let (asked, outcome) = run(
            true,
            None,
            Ok((
                1,
                "",
                format!("error opening file `/run/crun/{id}/status`: No such file or directory"),
            )),
        )
        .await;
        assert_eq!(asked, ["exec"]);
        assert!(
            outcome
                .as_ref()
                .is_err_and(|error| error.contains("the runtime has no such container")),
            "{outcome:?}"
        );
        // The command not ending in its budget fails, and asks nothing more.
        let (asked, outcome) = run(true, None, Err("did not finish within 60s")).await;
        assert_eq!(asked, ["exec"]);
        assert!(outcome.is_err(), "{outcome:?}");

        // No report from podman: the state decides, and only `running` runs.
        assert_eq!(
            run(false, Some(HookEligibility::Run), Ok((0, "", none()))).await,
            (vec!["state", "exec"], Ok(Some(Some(0))))
        );
        assert_eq!(
            run(
                false,
                Some(HookEligibility::Skip("created".to_string())),
                Ok((0, "", none()))
            )
            .await,
            (vec!["state"], Ok(None))
        );
        let (asked, outcome) = run(
            false,
            Some(HookEligibility::Fail("crun state exited 1".to_string())),
            Ok((0, "", none())),
        )
        .await;
        assert_eq!(asked, ["state"]);
        assert!(outcome.is_err(), "{outcome:?}");
    }

    /// The container-state read gets at least twice the slowest read measured in
    /// a restored guest, so a guest that is busy at restore is not failed closed
    /// for a read that was going to answer.
    ///
    /// RED BEFORE THE FIX: the budget was 10 s, and the first read after a
    /// restore of a busy 128 GiB guest took 27.4 s.
    #[test]
    fn the_inspect_budget_outlasts_a_busy_restored_guests_read() {
        assert!(
            HOOK_INSPECT_BUDGET >= SLOWEST_RESTORE_TIME_INSPECT * 2,
            "inspect budget {HOOK_INSPECT_BUDGET:?} is less than twice the slowest \
             measured read ({SLOWEST_RESTORE_TIME_INSPECT:?})"
        );
    }

    /// The decision from the runtime's answer: a running container runs the hook; a
    /// container in any other state, or one the runtime does not hold yet, skips
    /// it with the reason; the runtime failing for any other reason fails the
    /// restore closed, because a clone published without its stale-state repair is
    /// the failure the hook exists to prevent. So does a runtime that holds no
    /// container while podman's own report says one started: that is the hook
    /// asking the wrong runtime or state root, not a container yet to start.
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
        // crun 1.14.1's `state` of a running container, as a restored guest prints it.
        let state = |status: &str| {
            format!(
                "{{\n    \"ociVersion\": \"1.0.0\",\n    \"id\": \"{}\",\n    \"pid\": 812,\n    \
                 \"status\": \"{status}\",\n    \"bundle\": \"/b\",\n    \"owner\": \"root\"\n}}\n",
                "ab".repeat(32)
            )
        };
        for reported_running in [false, true] {
            assert_eq!(
                hook_eligibility(&output(0, &state("running"), ""), reported_running),
                HookEligibility::Run
            );
            for status in ["created", "stopped", "paused"] {
                assert_eq!(
                    hook_eligibility(&output(0, &state(status), ""), reported_running),
                    HookEligibility::Skip(format!(
                        "fcvm-container is not running (status={status})"
                    ))
                );
            }
            assert_eq!(
                hook_eligibility(&output(0, "{}", ""), reported_running),
                HookEligibility::Fail("crun state printed no status: {}".to_string())
            );
            assert_eq!(
                hook_eligibility(
                    &output(1, "", "open `/run/crun`: Permission denied"),
                    reported_running
                ),
                HookEligibility::Fail(
                    "crun state exited exit status: 1: open `/run/crun`: Permission denied"
                        .to_string()
                )
            );
        }
        // Both spellings of an id the runtime holds no state for.
        for unknown in [
            "error opening file `/run/crun/abab/status`: No such file or directory",
            "container `abab` does not exist",
        ] {
            assert_eq!(
                hook_eligibility(&output(1, "", unknown), false),
                HookEligibility::Skip(format!("crun state exited exit status: 1: {unknown}"))
            );
            assert_eq!(
                hook_eligibility(&output(1, "", unknown), true),
                HookEligibility::Fail(format!(
                    "podman reported fcvm-container started, and the runtime has no such \
                     container; crun state exited exit status: 1: {unknown}"
                ))
            );
        }
    }

    /// The hook's container is the one whose id the launch recorded. With none
    /// recorded podman has not created it and the hook is skipped, unless podman
    /// already reported it started; an unreadable or malformed record fails the
    /// restore, and never becomes a path or an argument.
    #[test]
    fn the_hook_runs_in_the_container_whose_id_the_launch_recorded() {
        let id = "0123456789abcdef".repeat(4);
        for reported_running in [false, true] {
            assert_eq!(
                hook_container_id(Ok(Some(format!("{id}\n"))), reported_running),
                Ok(id.clone())
            );
            for bad in [
                "",
                "fcvm-container",
                "../../etc",
                &id[..63],
                &id.to_uppercase(),
            ] {
                assert!(
                    matches!(
                        hook_container_id(Ok(Some(bad.to_string())), reported_running),
                        Err(HookEligibility::Fail(_))
                    ),
                    "{bad:?} was taken as a container id"
                );
            }
            assert!(matches!(
                hook_container_id(Err("cat exited 1".to_string()), reported_running),
                Err(HookEligibility::Fail(_))
            ));
        }
        assert_eq!(
            hook_container_id(Ok(None), false),
            Err(HookEligibility::Skip(
                "podman has not created fcvm-container".to_string()
            ))
        );
        assert!(matches!(
            hook_container_id(Ok(None), true),
            Err(HookEligibility::Fail(_))
        ));
    }

    /// The process `crun exec` runs for the hook is the container's own (its
    /// environment, working directory, user, rlimits and labels, as its runtime
    /// holds them) with the hook's command and no terminal, and with what podman
    /// gives an exec session in place of what the start gave the container: the
    /// session's capability sets, its supplementary groups, and no `HOSTNAME` the
    /// start added. The configs are what podman 4.9.3 wrote for a root container
    /// of an Alpine image and for a `--userns=keep-id` one, cut to their process.
    #[test]
    fn the_hook_process_is_a_podman_exec_sessions() {
        use crate::container::ExecSession;
        let args = ContainerRestoreHook {
            command: "mctile restored".to_string(),
        }
        .args();
        let session = |user_set, hostname_set| ExecSession {
            user_set,
            hostname_set,
        };
        let made = |config: &str, session| -> serde_json::Value {
            serde_json::from_str(&exec_process(config.as_bytes(), &args, session).unwrap()).unwrap()
        };
        let caps = r#"["CAP_CHOWN","CAP_KILL","CAP_NET_BIND_SERVICE"]"#;
        // No user set: the start gave root the groups Alpine's /etc/group lists for it.
        let root = format!(
            r#"{{"ociVersion":"1.1.0","process":{{"terminal":true,
            "user":{{"uid":0,"gid":0,"umask":18,"additionalGids":[0,1,2,3,4,6,10,11,20,26,27]}},
            "args":["sleep","600"],"env":["PATH=/usr/bin","container=podman","FOO=bar","HOME=/root","HOSTNAME=tile"],
            "cwd":"/srv","capabilities":{{"bounding":{caps},"effective":["CAP_KILL"],"permitted":["CAP_KILL"],"inheritable":["CAP_KILL"]}},
            "rlimits":[{{"type":"RLIMIT_NOFILE","hard":65536,"soft":65536}}],
            "apparmorProfile":"containers-default-0.57.4","oomScoreAdj":0,"noNewPrivileges":true}},
            "root":{{"path":"/merged"}},"hostname":"tile"}}"#
        );
        let config: serde_json::Value = serde_json::from_str(&root).unwrap();
        let process = made(&root, session(Some(false), Some(false)));
        assert_eq!(process["args"], serde_json::json!(args));
        assert_eq!(process["terminal"], false);
        for kept in [
            "cwd",
            "rlimits",
            "apparmorProfile",
            "oomScoreAdj",
            "noNewPrivileges",
        ] {
            assert_eq!(process[kept], config["process"][kept], "{kept}");
        }
        let caps: serde_json::Value = serde_json::from_str(caps).unwrap();
        assert_eq!(process["capabilities"]["bounding"], caps);
        assert_eq!(process["capabilities"]["effective"], caps);
        assert_eq!(process["capabilities"]["permitted"], caps);
        assert_eq!(
            process["capabilities"]["inheritable"],
            serde_json::json!([])
        );
        assert!(process["capabilities"].get("ambient").is_none());
        // A session of a container with no user has no supplementary groups, and no
        // HOSTNAME that only the start set.
        assert_eq!(
            process["user"],
            serde_json::json!({"uid": 0, "gid": 0, "umask": 18})
        );
        assert_eq!(
            process["env"],
            serde_json::json!(["PATH=/usr/bin", "container=podman", "FOO=bar", "HOME=/root"])
        );
        // A HOSTNAME the image or the plan set is the session's too, and one whose
        // origin is unknown stays, as the container's own processes have it.
        for hostname_set in [Some(true), None] {
            assert_eq!(
                made(&root, session(Some(false), hostname_set))["env"],
                config["process"]["env"],
                "{hostname_set:?}"
            );
        }
        // An image that names root as its user: the session has root's groups, less
        // the primary one the start added.
        assert_eq!(
            made(&root, session(Some(true), Some(false)))["user"]["additionalGids"],
            serde_json::json!([1, 2, 3, 4, 6, 10, 11, 20, 26, 27])
        );
        // Unknown takes none: never more groups than a session has.
        assert!(made(&root, session(None, None))["user"]
            .get("additionalGids")
            .is_none());

        // A container user other than root holds what the container's effective set
        // holds, here nothing: no exec session of it gets the bounding set. Its
        // primary group is not among a session's supplementary groups.
        let user = r#"{"process":{"user":{"uid":1000,"gid":1000,"umask":18,"additionalGids":[1000,44]},
            "args":["sleep","600"],"env":["HOME=/home/app"],"cwd":"/",
            "capabilities":{"bounding":["CAP_CHOWN","CAP_KILL"]}}}"#;
        let process = made(user, session(Some(true), Some(false)));
        assert_eq!(
            process["user"],
            serde_json::json!({"uid": 1000, "gid": 1000, "umask": 18, "additionalGids": [44]})
        );
        assert_eq!(
            process["capabilities"]["bounding"],
            serde_json::json!(["CAP_CHOWN", "CAP_KILL"])
        );
        for set in ["effective", "permitted", "inheritable", "ambient"] {
            assert_eq!(process["capabilities"][set], serde_json::json!([]), "{set}");
        }

        for broken in ["", "{}", r#"{"process": "sleep"}"#] {
            assert!(
                exec_process(broken.as_bytes(), &args, ExecSession::default()).is_err(),
                "{broken:?}"
            );
        }
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
                "syn-cookies-off",
                "gate-verify",
                "gate-udp-ingress",
                "rx-barrier",
                "cookie-dump",
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
    /// hangs ends inside the container too, not only at the runtime's client.
    #[test]
    fn the_hook_runs_under_the_containers_own_timeout() {
        let hook = ContainerRestoreHook {
            command: "curl -sf http://[::1]:8088/x".to_string(),
        };
        assert_eq!(
            hook.args(),
            [
                "timeout",
                "-k",
                &RESTORE_HOOK_KILL_AFTER_SECS.to_string(),
                &RESTORE_HOOK_COMMAND_SECS.to_string(),
                "sh",
                "-c",
                "curl -sf http://[::1]:8088/x",
            ]
        );
        // The process handed to `crun exec` is the one the id names, in the state
        // root podman's crun uses.
        let id = "cd".repeat(32);
        let runtime = crate::container::ContainerRuntime::new(None, None);
        assert_eq!(
            ContainerRestoreHook::exec_argv(&runtime, &id, std::path::Path::new("/run/p.json")),
            [
                "crun",
                "--root",
                "/run/crun",
                "exec",
                "--process",
                "/run/p.json",
                id.as_str()
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
                "syn-cookies-off",
                "gate-verify",
                "gate-udp-ingress",
                "rx-barrier",
                "cookie-dump",
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
                "syn-cookies-off",
                "gate-verify",
                "gate-udp-ingress",
                "rx-barrier",
                "cookie-dump",
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
                "syn-cookies-off",
                "gate-verify",
                "gate-udp-ingress",
                "rx-barrier",
                "cookie-dump",
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
                unlisted: 0,
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

    /// #1090: one restore spent 16 s inside `tcp_cleanup_ms` that no sub-timer
    /// held. The lock wait, the constructors and the console write are the steps
    /// outside the boundary's timers, each made slow here by a known amount, and
    /// each must land in its own field of the record rather than in the phase's
    /// remainder.
    #[tokio::test]
    async fn the_locked_boundary_times_the_lock_wait_the_constructors_and_the_console_write() {
        const LOCK_HELD_MS: u64 = 150;
        const CONSTRUCT_MS: u64 = 30;
        const CONSOLE_MS: u64 = 40;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("snapshot-network.lock");
        let held = acquire_boundary_lock_at(&path).unwrap();
        // The holder lets go a fixed time after the acquirer has begun to wait,
        // so the wait is at least that long whatever the scheduler does.
        let (attempting_tx, attempting_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            attempting_rx.recv().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(LOCK_HELD_MS));
            drop(held);
        });
        let state = Arc::new(Mutex::new(ModelState {
            gate_closed: true,
            egress_closed: true,
            ..Default::default()
        }));
        let kernel = SharedKernel::new(&[], ModelAnswer::Kernel, ModelDelivery::InOrder);
        let phase_started = std::time::Instant::now();
        let (_lock, _gate, report) = restore_locked_boundary_with(
            || {
                attempting_tx.send(()).unwrap();
                acquire_boundary_lock_at(&path)
            },
            || {
                std::thread::sleep(std::time::Duration::from_millis(CONSTRUCT_MS));
                let store = MemoryStore::default();
                store
                    .save(&SnapshotNetworkManifest {
                        version: MANIFEST_VERSION,
                        routes: Vec::new(),
                        sockets: Vec::new(),
                    })
                    .unwrap();
                (
                    ModelGate(state.clone()),
                    ModelBarrier(state.clone()),
                    ChannelDiag(CookieDestroyer::new(kernel.opener())),
                    store,
                )
            },
            CLEANUP_BUDGET,
            || async { Ok(()) },
            |_| std::thread::sleep(std::time::Duration::from_millis(CONSOLE_MS)),
        )
        .await
        .unwrap();
        let cleanup_ms = phase_started.elapsed().as_secs_f64() * 1000.0;
        holder.join().unwrap();

        assert!(
            report.lock_wait_ms >= LOCK_HELD_MS as f64,
            "the lock was held {LOCK_HELD_MS} ms, the report says {} ms",
            report.lock_wait_ms
        );
        assert!(
            report.construct_ms >= CONSTRUCT_MS as f64,
            "the constructors took {CONSTRUCT_MS} ms, the report says {} ms",
            report.construct_ms
        );
        assert!(
            report.complete_console_ms >= CONSOLE_MS as f64,
            "the console write took {CONSOLE_MS} ms, the report says {} ms",
            report.complete_console_ms
        );

        let mut phases = crate::restore::RestorePhases {
            tcp_cleanup_ms: cleanup_ms,
            ..Default::default()
        };
        phases.record_boundary(&report);
        let accounted = phases.tcp_cleanup_accounted_ms();
        assert!(
            accounted >= (LOCK_HELD_MS + CONSTRUCT_MS + CONSOLE_MS) as f64,
            "the timed parts hold {accounted} ms of a {cleanup_ms} ms phase"
        );
        assert!(
            accounted <= cleanup_ms + 1e-6,
            "the timed parts ({accounted} ms) overlap: they exceed the phase ({cleanup_ms} ms)"
        );
        let record: serde_json::Value = serde_json::from_str(&phases.to_frame_json()).unwrap();
        assert!(
            record["tcp_lock_wait_ms"].as_f64().unwrap() >= LOCK_HELD_MS as f64,
            "{record}"
        );
        assert!(
            record["tcp_cleanup_untimed_ms"].as_f64().unwrap() < accounted,
            "the held lock must not be left in the remainder: {record}"
        );
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

    /// The knob each barrier test's grace period reads, and what it read.
    /// One pair per test: a `fn` pointer cannot capture, and tests run in
    /// parallel threads of one binary.
    struct KnobProbe {
        knob: std::sync::OnceLock<PathBuf>,
        seen: Mutex<Option<String>>,
    }

    impl KnobProbe {
        const fn new() -> Self {
            Self {
                knob: std::sync::OnceLock::new(),
                seen: Mutex::new(None),
            }
        }

        fn observe(&self) -> Result<()> {
            let text = std::fs::read_to_string(self.knob.get().unwrap())
                .unwrap_or_else(|error| format!("unreadable: {error}"));
            *self.seen.lock().unwrap() = Some(text);
            Ok(())
        }
    }

    /// The receive barrier's grace period is an expedited one. The guest
    /// kernel ticks at 100 Hz, and a normal grace period behind it was the
    /// largest step of a restore before publication and sat inside the
    /// source's gated window (see `ExpeditedGracePeriods`). The kernel's
    /// `rcu_expedited` switch is on while the barrier waits and back to its
    /// old value after; the guarantee (every RCU reader that began before has
    /// finished) is the same either way.
    #[test]
    fn the_receive_barrier_waits_an_expedited_grace_period() {
        static PROBE: KnobProbe = KnobProbe::new();
        let dir = tempfile::tempdir().unwrap();
        let knob = dir.path().join("rcu_expedited");
        std::fs::write(&knob, "0\n").unwrap();
        PROBE.knob.set(knob.clone()).unwrap();
        let barrier = SystemPacketReceiveBarrier {
            expedite_knob: knob.clone(),
            expedite_record: dir.path().join("before"),
            grace_period: || PROBE.observe(),
        };
        barrier.synchronize().unwrap();
        assert_eq!(
            PROBE.seen.lock().unwrap().as_deref().map(str::trim),
            Some("1"),
            "the barrier waited a normal grace period: rcu_expedited was not on while it waited"
        );
        assert_eq!(
            std::fs::read_to_string(&knob).unwrap().trim(),
            "0",
            "the barrier left the kernel expediting every grace period"
        );
    }

    /// A switch someone else turned on stays on, and a kernel without the
    /// switch still gets its barrier: expediting is an optimisation, the grace
    /// period is the guarantee.
    #[test]
    fn the_receive_barrier_keeps_a_switch_already_on_and_runs_without_one() {
        static ON: KnobProbe = KnobProbe::new();
        let dir = tempfile::tempdir().unwrap();
        let knob = dir.path().join("rcu_expedited");
        std::fs::write(&knob, "1\n").unwrap();
        ON.knob.set(knob.clone()).unwrap();
        SystemPacketReceiveBarrier {
            expedite_knob: knob.clone(),
            expedite_record: dir.path().join("before"),
            grace_period: || ON.observe(),
        }
        .synchronize()
        .unwrap();
        assert_eq!(ON.seen.lock().unwrap().as_deref().map(str::trim), Some("1"));
        assert_eq!(std::fs::read_to_string(&knob).unwrap().trim(), "1");

        static ABSENT: KnobProbe = KnobProbe::new();
        let missing = dir.path().join("absent");
        ABSENT.knob.set(missing.clone()).unwrap();
        SystemPacketReceiveBarrier {
            expedite_knob: missing.clone(),
            expedite_record: dir.path().join("before"),
            grace_period: || ABSENT.observe(),
        }
        .synchronize()
        .unwrap();
        assert!(
            ABSENT.seen.lock().unwrap().is_some(),
            "no grace period without the switch"
        );
        assert!(
            !missing.exists(),
            "the barrier created a knob that was not there"
        );
    }

    /// A barrier that is killed between its two writes of `rcu_expedited` runs no drop
    /// guard, and the switch stayed on for the rest of the guest's life and in every
    /// later snapshot of it (#2141). The value to put back is recorded before the switch
    /// is turned on, and the next barrier, resume or agent start puts it back.
    ///
    /// RED BEFORE THE FIX: the next barrier found the switch on, took it for someone
    /// else's and left it.
    #[test]
    fn a_barrier_killed_with_rcu_expedited_on_is_undone_by_the_next_one() {
        let dir = tempfile::tempdir().unwrap();
        let knob = dir.path().join("rcu_expedited");
        let record = dir.path().join("run").join("before");
        std::fs::write(&knob, "0\n").unwrap();

        // killed inside the barrier: the guard never drops
        std::mem::forget(ExpeditedGracePeriods::begin(&knob, &record).unwrap());
        assert_eq!(std::fs::read_to_string(&knob).unwrap().trim(), "1");
        assert_eq!(std::fs::read_to_string(&record).unwrap().trim(), "0");

        // the next barrier
        static NEXT: KnobProbe = KnobProbe::new();
        NEXT.knob.set(knob.clone()).unwrap();
        SystemPacketReceiveBarrier {
            expedite_knob: knob.clone(),
            expedite_record: record.clone(),
            grace_period: || NEXT.observe(),
        }
        .synchronize()
        .unwrap();
        assert_eq!(
            NEXT.seen.lock().unwrap().as_deref().map(str::trim),
            Some("1"),
            "the barrier after a killed one did not expedite"
        );
        assert_eq!(
            std::fs::read_to_string(&knob).unwrap().trim(),
            "0",
            "the killed barrier's switch is still on"
        );
        assert!(!record.exists(), "a record outlived its barrier");

        // a resume or an agent start, with no barrier of its own
        std::mem::forget(ExpeditedGracePeriods::begin(&knob, &record).unwrap());
        ExpeditedGracePeriods::put_back(&knob, &record);
        assert_eq!(std::fs::read_to_string(&knob).unwrap().trim(), "0");
        assert!(!record.exists());

        // No record: a switch someone else turned on is theirs.
        std::fs::write(&knob, "1\n").unwrap();
        ExpeditedGracePeriods::put_back(&knob, &record);
        assert_eq!(std::fs::read_to_string(&knob).unwrap().trim(), "1");

        // Each place that has to put it back does.
        let source = include_str!("snapshot_network.rs");
        // The resume puts it back before it tries the gate and whatever comes of that: a
        // reopen that never lands must not leave the switch on for the next snapshot.
        let resume = function_body(source, "async fn resume_source_network_with<");
        assert!(
            resume.find("ExpeditedGracePeriods::put_back(").unwrap()
                < resume.find("reopen_owed_gate(").unwrap()
        );
        assert!(!function_body(source, "async fn reopen_owed_gate<").contains("put_back"));
        assert!(!function_body(source, "fn retire_source_boundary<").contains("put_back"));
        let main = include_str!("main.rs");
        let put_back = main
            .find("snapshot_network::put_back_rcu_expedited_after_a_kill();")
            .expect("the agent's start does not put rcu_expedited back");
        assert!(put_back < main.find("agent::run()").unwrap());
    }

    fn marks_in(dir: &Path) -> BoundaryMarks {
        BoundaryMarks {
            owed: dir.join("run").join("owed"),
            restore: dir.join("run").join("restore"),
        }
    }

    /// A resume that cannot open the gate leaves the boundary owed: the manifest and the
    /// `owed` mark stay and the caller does not thaw, so the next resume finds the
    /// boundary as this one did (#2142). One that opens it clears both.
    ///
    /// RED BEFORE THE FIX: the manifest was removed and the container thawed after a
    /// failed reopen, and nothing in the guest said the gate was still closed.
    #[tokio::test]
    async fn a_resume_that_cannot_open_the_gate_leaves_the_boundary_owed() {
        let dir = tempfile::tempdir().unwrap();
        let marks = marks_in(dir.path());
        BoundaryMarks::mark(&marks.owed).unwrap();
        let store = MemoryStore::default();
        store
            .save(&SnapshotNetworkManifest {
                version: MANIFEST_VERSION,
                routes: Vec::new(),
                sockets: Vec::new(),
            })
            .unwrap();
        let state = Arc::new(Mutex::new(ModelState {
            gate_closed: true,
            egress_closed: true,
            failing_gate_opens: REOPEN_TRIES,
            ..Default::default()
        }));
        let gate = ModelGate(state.clone());

        let reopened = reopen_owed_gate(&gate, std::time::Duration::ZERO).await;
        assert_eq!(
            state.lock().unwrap().events,
            vec!["gate-open-failed"; REOPEN_TRIES as usize],
            "the reopen is tried a bounded number of times"
        );
        let resumed = retire_source_boundary(reopened, &store, &marks);
        assert!(
            !resumed.gate_open,
            "the caller would thaw behind a closed gate"
        );
        let error = format!("{:#}", resumed.result.unwrap_err());
        assert!(error.contains("still owed"), "{error}");
        assert!(
            store.load().is_ok(),
            "the manifest went with the gate closed"
        );
        assert!(marks.source_owed());
        assert!(state.lock().unwrap().gate_closed);

        // The host runs the one-shot again, and the gate opens on its second try.
        state.lock().unwrap().failing_gate_opens = 1;
        let reopened = reopen_owed_gate(&gate, std::time::Duration::ZERO).await;
        let resumed = retire_source_boundary(reopened, &store, &marks);
        assert!(resumed.gate_open);
        resumed.result.unwrap();
        assert!(store.load().is_err());
        assert!(!marks.source_owed());
        assert!(!state.lock().unwrap().gate_closed);

        // What the one-shot's caller does with that.
        let source = include_str!("snapshot_network.rs");
        let resume = function_body(source, "async fn resume_source_network_with<");
        let reopen = resume.find("reopen_owed_gate(gate, REOPEN_PAUSE)").unwrap();
        let retire = resume
            .find("retire_source_boundary(reopened, store, marks)")
            .unwrap();
        assert!(reopen < retire);
        assert!(
            !resume.contains(".remove()"),
            "the resume retires the manifest somewhere other than `retire_source_boundary`"
        );
    }

    /// The prepare's own order, and what it refuses.
    ///
    /// - A guest whose restore is under way refuses the prepare, before the freeze: the
    ///   gate, the manifest and the frozen container are the restore's (#2138). The
    ///   refusal says it froze nothing, so its caller thaws nothing.
    /// - `owed` is written before the freeze and before the gate, so an agent that dies
    ///   anywhere in the prepare leaves a boundary the next resume is owed.
    /// - A prepare that fails after its freeze says so, and keeps `owed` for the resume
    ///   the host sends after a failed prepare.
    ///
    /// RED BEFORE THE FIX: the prepare froze the container before it looked, ran on a
    /// restoring clone as on any source, and thawed the restore's freeze when it failed.
    #[tokio::test]
    async fn a_prepare_is_refused_while_a_restore_is_under_way_and_freezes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let marks = marks_in(dir.path());
        let lock = dir.path().join("run").join("lock");
        let events = Mutex::new(Vec::new());
        // What a read of the gate finds after a failed prepare: installed, gone, or no
        // answer.
        let gate = Mutex::new(Some(false));
        let run = |fail: bool| {
            let events = &events;
            let marks = &marks;
            let lock = &lock;
            let gate = &gate;
            async move {
                prepare_snapshot_network_in(
                    marks,
                    lock,
                    Some("create-a"),
                    || {
                        assert!(marks.owed.exists(), "frozen before the boundary is owed");
                        events.lock().unwrap().push("freeze");
                    },
                    || async {
                        match *gate.lock().unwrap() {
                            Some(installed) => Ok(installed),
                            None => bail!("injected gate read failure"),
                        }
                    },
                    || async {
                        events.lock().unwrap().push("boundary");
                        if fail {
                            bail!("injected boundary failure");
                        }
                        Ok(())
                    },
                )
                .await
            }
        };

        begin_restore_boundary_in(&marks).unwrap();
        let refused = run(false).await.unwrap_err();
        assert!(
            !refused.thaw,
            "a refused prepare's caller would thaw the restore's freeze"
        );
        assert!(refused.error.downcast_ref::<RestoreUnderWay>().is_some());
        assert!(
            events.lock().unwrap().is_empty(),
            "a refused prepare did something"
        );
        assert!(
            !marks.owed.exists(),
            "a refused prepare left a boundary owed"
        );

        // The restore published.
        BoundaryMarks::clear(&marks.restore).unwrap();
        run(false).await.unwrap();
        assert_eq!(*events.lock().unwrap(), vec!["freeze", "boundary"]);
        assert!(marks.source_owed());

        events.lock().unwrap().clear();
        // The prepare's rollback removed the gate: the caller thaws.
        let failed = run(true).await.unwrap_err();
        assert!(failed.thaw);
        assert!(failed.error.downcast_ref::<RestoreUnderWay>().is_none());
        assert!(
            marks.source_owed(),
            "a failed prepare's boundary is still owed its resume"
        );
        // The gate is still installed, or cannot be read: the container stays frozen for
        // the resume that opens it.
        // RED BEFORE THE FIX: a failed prepare thawed whatever the gate's state.
        for still_closed in [Some(true), None] {
            *gate.lock().unwrap() = still_closed;
            let failed = run(true).await.unwrap_err();
            assert!(
                !failed.thaw,
                "thawed behind a gate read as {still_closed:?}"
            );
        }
        *gate.lock().unwrap() = Some(false);

        // A restore dispatched while the prepare waits for the boundary lock.
        events.lock().unwrap().clear();
        BoundaryMarks::clear(&marks.owed).unwrap();
        let held = acquire_boundary_lock_at(&lock).unwrap();
        let restore = marks.restore.clone();
        let dispatched = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            BoundaryMarks::mark(&restore).unwrap();
            drop(held);
        });
        let refused = run(false).await.unwrap_err();
        dispatched.join().unwrap();
        assert!(!refused.thaw);
        assert!(refused.error.downcast_ref::<RestoreUnderWay>().is_some());
        assert!(events.lock().unwrap().is_empty());
        assert!(!marks.owed.exists());
    }

    /// The container's start waits out a boundary, and no longer.
    ///
    /// - With no mark it waits for nothing.
    /// - A restore under way holds it until the restore publishes.
    /// - A source's `owed` mark holds it only while the gate is installed. A mark whose
    ///   gate is gone (a failed prepare's rollback opened it and the host died, or a
    ///   resume was killed between the gate and the mark) holds nothing.
    /// - With the gate closed, a look while someone holds the boundary lock is not
    ///   counted and starts the count again; `HOLD_ABANDONED_LOOKS` counted looks in a
    ///   row end the hold as abandoned. At most `UNCOUNTED_LOOKS_CAP` looks go uncounted.
    ///
    /// RED BEFORE THE FIX: nothing in the agent's start looked at the boundary. Then: a
    /// stale `owed` mark with the gate open, and a closed gate nobody would ever open,
    /// each held the container's start for ever.
    #[tokio::test]
    async fn the_containers_start_waits_for_the_boundary_to_be_over() {
        let dir = tempfile::tempdir().unwrap();
        let marks = marks_in(dir.path());
        let lock = dir.path().join("run").join("lock");
        let look = std::time::Duration::from_millis(1);
        let gate_closed = std::sync::atomic::AtomicBool::new(true);
        let looks = AtomicU32::new(0);
        let hold = || {
            hold_the_container_start_with(
                &marks,
                &lock,
                || async {
                    looks.fetch_add(1, Ordering::SeqCst);
                    Ok(gate_closed.load(Ordering::SeqCst))
                },
                look,
            )
        };
        let within = |limit_ms: u64| std::time::Duration::from_millis(limit_ms);

        // no boundary: at once, and the gate is not even read
        assert_eq!(hold().await, HoldEnd::Over);
        assert_eq!(looks.load(Ordering::SeqCst), 0);

        // a stale mark, its gate open: nothing to wait for
        BoundaryMarks::mark(&marks.owed).unwrap();
        gate_closed.store(false, Ordering::SeqCst);
        assert_eq!(
            tokio::time::timeout(within(5_000), hold()).await.unwrap(),
            HoldEnd::Over
        );

        // a prepare's gate, a boundary someone is working on: held, and not counted. The
        // gate opens at a look past the count that would have abandoned it.
        gate_closed.store(true, Ordering::SeqCst);
        let working = acquire_boundary_lock_at(&lock).unwrap();
        looks.store(0, Ordering::SeqCst);
        let end = hold_the_container_start_with(
            &marks,
            &lock,
            || async { Ok(looks.fetch_add(1, Ordering::SeqCst) < HOLD_ABANDONED_LOOKS + 100) },
            std::time::Duration::ZERO,
        )
        .await;
        assert_eq!(
            end,
            HoldEnd::Over,
            "the hold gave up on a boundary someone is working on"
        );
        assert_eq!(looks.load(Ordering::SeqCst), HOLD_ABANDONED_LOOKS + 101);
        drop(working);
        // ... until the resume opens the gate
        let opened = async {
            tokio::time::sleep(within(30)).await;
            gate_closed.store(false, Ordering::SeqCst);
        };
        let (end, ()) = tokio::time::timeout(within(5_000), async { tokio::join!(hold(), opened) })
            .await
            .unwrap();
        assert_eq!(end, HoldEnd::Over);

        // the gate closed and nobody working on it: abandoned after the counted looks
        gate_closed.store(true, Ordering::SeqCst);
        looks.store(0, Ordering::SeqCst);
        assert_eq!(
            tokio::time::timeout(within(30_000), hold()).await.unwrap(),
            HoldEnd::Abandoned
        );
        assert_eq!(looks.load(Ordering::SeqCst), HOLD_ABANDONED_LOOKS);

        // Someone works on the boundary at the look before the count would run out (a
        // later snapshot create's prepare): the count starts again, so the hold is not
        // abandoned inside that create's boundary.
        let cap = crate::payload_freeze::UNCOUNTED_LOOKS_CAP;
        let working: std::sync::Mutex<Option<std::fs::File>> = std::sync::Mutex::new(None);
        looks.store(0, Ordering::SeqCst);
        let end = hold_the_container_start_with(
            &marks,
            &lock,
            || async {
                let look = looks.fetch_add(1, Ordering::SeqCst) + 1;
                let mut working = working.lock().unwrap();
                if look == HOLD_ABANDONED_LOOKS {
                    *working = Some(acquire_boundary_lock_at(&lock).unwrap());
                } else {
                    *working = None;
                }
                Ok(true)
            },
            std::time::Duration::ZERO,
        )
        .await;
        assert_eq!(end, HoldEnd::Abandoned);
        assert_eq!(
            looks.load(Ordering::SeqCst),
            2 * HOLD_ABANDONED_LOOKS,
            "the looks counted before someone worked on the boundary still counted"
        );

        // A one-shot hung under the lock: its looks go uncounted up to the cap, then
        // count, so the hold ends.
        let hung = acquire_boundary_lock_at(&lock).unwrap();
        looks.store(0, Ordering::SeqCst);
        let end = tokio::time::timeout(
            within(60_000),
            hold_the_container_start_with(
                &marks,
                &lock,
                || async {
                    looks.fetch_add(1, Ordering::SeqCst);
                    Ok(true)
                },
                std::time::Duration::ZERO,
            ),
        )
        .await
        .expect("a lock held for ever held the container's start for ever");
        assert_eq!(end, HoldEnd::Abandoned);
        assert_eq!(looks.load(Ordering::SeqCst), cap + HOLD_ABANDONED_LOOKS);
        drop(hung);

        // the image's clone: the restore takes the boundary, and the start waits for its
        // publication whatever the gate
        begin_restore_boundary_in(&marks).unwrap();
        assert!(!marks.owed.exists());
        tokio::time::timeout(within(100), hold())
            .await
            .expect_err("the container started in the middle of a restore");
        let published = async {
            tokio::time::sleep(within(30)).await;
            BoundaryMarks::clear(&marks.restore).unwrap();
        };
        let (end, ()) =
            tokio::time::timeout(within(5_000), async { tokio::join!(hold(), published) })
                .await
                .unwrap();
        assert_eq!(end, HoldEnd::Over);

        // The hold outlasts the host's asking: no create is in flight when it gives up.
        let host = include_str!("../../src/commands/common.rs");
        assert!(host.contains(
            "const SOURCE_REOPEN_BOUND: std::time::Duration = std::time::Duration::from_secs(150);"
        ));
        assert!(u64::from(HOLD_ABANDONED_LOOKS) * HOLD_LOOK.as_secs() >= 2 * 150);
    }

    /// An agent that starts with a source's boundary owed writes that into the mark, and
    /// the resume that opens the gate reports it on the stderr the host reads: the image
    /// the host took may hold a dead agent and a killed container, and only the guest
    /// knows. The note never makes a mark, and goes with it.
    ///
    /// RED BEFORE THE FIX: a snapshot whose source's agent died in the boundary was
    /// published like any other.
    #[tokio::test]
    async fn an_agent_that_restarts_in_a_boundary_says_so_and_the_resume_reports_it() {
        let dir = tempfile::tempdir().unwrap();
        let marks = marks_in(dir.path());
        assert!(marks.note_agent_restarted().is_err());
        assert!(!marks.owed.exists(), "the note made a mark");
        assert!(!marks.agent_restarted());

        BoundaryMarks::mark(&marks.owed).unwrap();
        assert!(!marks.agent_restarted());
        marks.note_agent_restarted().unwrap();
        assert!(marks.agent_restarted());
        assert!(marks.source_owed());
        BoundaryMarks::clear(&marks.owed).unwrap();
        assert!(!marks.agent_restarted());

        // The agent notes it where it opens exec for the resume alone, and the resume
        // reads the note before it retires the mark.
        let agent = include_str!("agent.rs").replace(char::is_whitespace, "");
        let noted = agent
            .find("crate::snapshot_network::note_the_agent_restarted_in_a_boundary();")
            .expect("the agent notes its restart");
        assert!(
            noted
                < agent
                    .find("exec::serve_only_the_boundary_resume();")
                    .unwrap()
        );
        let source = include_str!("snapshot_network.rs");
        let resume = &source[source.find("async fn resume_source_network_with<").unwrap()..];
        let resume = &resume[..resume.find("\nasync fn reopen_owed_gate<").unwrap()];
        let read = resume.find("marks.agent_restarted()").unwrap();
        let retired = resume
            .find("retire_source_boundary(reopened, store, marks)")
            .unwrap();
        let reported = resume
            .find("eprintln!(\"[fc-agent] {AGENT_RESTARTED_REPORT}\")")
            .unwrap();
        assert!(read < retired && retired < reported);
    }

    /// The gate is a chain or a jump to one, in a rules listing: not the chain's name in
    /// some other rule's comment, which held a restarted agent's container start for 300
    /// looks on an open gate.
    #[test]
    fn a_gate_is_a_chain_or_a_jump_to_one_not_a_name_in_a_comment() {
        for gate in [
            "-N FCVM_SNAPSHOT_IN",
            "-P INPUT ACCEPT\n-A INPUT -j FCVM_SNAPSHOT_IN",
            "-A OUTPUT -p tcp -j FCVM_SNAPSHOT_OUT",
            "-A FCVM_SNAPSHOT_OUT -p tcp -j REJECT --reject-with tcp-reset",
            "-A INPUT -g FCVM_SNAPSHOT_IN",
        ] {
            assert!(rules_hold_the_gate(gate), "{gate}");
        }
        for not_gate in [
            "",
            "-P INPUT ACCEPT\n-P OUTPUT ACCEPT",
            "-A INPUT -m comment --comment FCVM_SNAPSHOT_IN -j ACCEPT",
            "-A INPUT -m comment --comment \"before FCVM_SNAPSHOT_OUT\" -j ACCEPT",
            "-N FCVM_SNAPSHOT_INNER\n-A INPUT -j FCVM_SNAPSHOT_INNER",
        ] {
            assert!(!rules_hold_the_gate(not_gate), "{not_gate}");
        }
    }

    /// A restore whose `restore-under-way` mark cannot be removed is not acknowledged:
    /// with the mark left, the clone would refuse every snapshot for the rest of its
    /// life while its host believed it restored. A mark that is already gone is fine.
    ///
    /// RED BEFORE THE FIX: the failure was a warning, and the acknowledgement followed.
    #[test]
    fn a_restore_whose_mark_cannot_be_removed_is_not_acknowledged() {
        let dir = tempfile::tempdir().unwrap();
        let marks = marks_in(dir.path());
        end_restore_boundary_in(&marks).expect("a mark already gone is removed");
        begin_restore_boundary_in(&marks).unwrap();
        end_restore_boundary_in(&marks).unwrap();
        assert!(!marks.restore_under_way());
        // a mark that cannot be removed: a directory with something in it
        std::fs::create_dir_all(marks.restore.join("held")).unwrap();
        assert!(end_restore_boundary_in(&marks).is_err());

        let watcher: String = include_str!("mmds.rs").split_whitespace().collect();
        let ended = watcher
            .find("ifletErr(error)=crate::snapshot_network::end_restore_boundary(){signals.restore_status.fail();")
            .expect("the watcher does not fail the restore on the mark");
        assert!(
            ended
                < watcher
                    .find("crate::vsock::notify_restore_complete(current,&phases)")
                    .unwrap()
        );
    }

    /// The resume looks for a restore once more after its last wait and before it touches
    /// the gate, and what follows an open gate is done whatever became of the cleanup.
    ///
    /// - A restore takes the boundary at its dispatch without the lock, so one dispatched
    ///   while a resume waited for the console's release found the resume already past its
    ///   checks: the resume then opened the gate and retired the manifest of a clone in
    ///   the middle of its restore.
    /// - A resume that opened the gate and could not clear `owed` returned before it asked
    ///   the output writer to reconnect, and the container's output after the snapshot was
    ///   lost.
    #[test]
    fn a_resume_looks_for_a_restore_before_the_gate_and_recovers_behind_an_open_one() {
        let source = include_str!("snapshot_network.rs");
        let resume = &source[source.find("async fn resume_source_network_with<").unwrap()..];
        let resume = &resume[..resume.find("\nasync fn reopen_owed_gate<").unwrap()];
        let released = resume
            .find("crate::console::request_source_release().await")
            .unwrap();
        let offline = resume.find("chronyc(&[\"offline\"]).await").unwrap();
        let looked = resume.rfind("if marks.restore_under_way() {").unwrap();
        let reopened = resume
            .find("reopen_owed_gate(gate, REOPEN_PAUSE).await")
            .unwrap();
        assert!(released < looked && offline < looked && looked < reopened);
        let closed = resume
            .find("if !resumed.gate_open {\n        return resumed;")
            .unwrap();
        assert!(!resume.contains("if resumed.result.is_err() {"));
        assert!(closed < resume.find("crate::output::request_reconnect()").unwrap());
        assert!(closed < resume.find("chrony_burst().await").unwrap());
    }

    /// A resume acts only on the boundary it was sent for (#2159).
    ///
    /// The host gives every snapshot create a token. The prepare writes it into the
    /// `owed` mark and every resume of that create carries it. A resume with another
    /// create's token, or with none where the mark has one, changes nothing: the resume
    /// itself is not run, the result says the gate is not open (so the one-shot thaws
    /// nothing), and the mark is as it was.
    ///
    /// The case this is for: a resume the host gave up on, delivered late, between a
    /// later create's prepare and its pause.
    ///
    /// A clone inherits its source's mark, token included, until its restore is
    /// dispatched and clears it. Before that a resume with the source's token would be
    /// obeyed, as any resume was before; no path sends one (the source's host addresses
    /// the source's vsock, and a create of the clone carries a token of its own, which
    /// this refuses). After the dispatch both one-shots are refused for the restore.
    ///
    /// RED BEFORE THE FIX: every resume acted on whatever boundary was owed.
    #[tokio::test]
    async fn a_resume_acts_only_on_the_boundary_it_was_sent_for() {
        let dir = tempfile::tempdir().unwrap();
        let marks = marks_in(dir.path());
        let lock = dir.path().join("run").join("lock");
        let ran = AtomicU32::new(0);
        let resume = |token: Option<&'static str>| {
            resume_source_network_in(&marks, &lock, token, || async {
                ran.fetch_add(1, Ordering::SeqCst);
                SourceResume {
                    gate_open: true,
                    result: Ok(()),
                }
            })
        };
        let refused = |resumed: SourceResume| {
            !resumed.gate_open
                && resumed
                    .result
                    .is_err_and(|error| error.downcast_ref::<StaleResume>().is_some())
        };

        // create B's prepare; create A's resume arrives late, and so does one with no token
        marks.mark_owed(Some("create-b")).unwrap();
        let mark = std::fs::read_to_string(&marks.owed).unwrap();
        assert!(refused(resume(Some("create-a")).await));
        assert!(refused(resume(None).await));
        assert_eq!(ran.load(Ordering::SeqCst), 0, "a stale resume ran");
        assert_eq!(std::fs::read_to_string(&marks.owed).unwrap(), mark);
        // B's own resume, also after its agent restarted in the boundary
        marks.note_agent_restarted().unwrap();
        assert!(marks.agent_restarted());
        assert_eq!(marks.owed_token().as_deref(), Some("create-b"));
        assert!(resume(Some("create-b")).await.gate_open);
        assert_eq!(ran.load(Ordering::SeqCst), 1);

        // a prepare with no token (run by hand): only a resume with none
        marks.mark_owed(None).unwrap();
        assert!(refused(resume(Some("create-a")).await));
        assert!(resume(None).await.gate_open);

        // nothing owed: a resume opens nothing a boundary relies on, whatever it carries
        BoundaryMarks::clear(&marks.owed).unwrap();
        assert!(resume(Some("create-a")).await.gate_open);
        assert!(resume(None).await.gate_open);
        assert_eq!(ran.load(Ordering::SeqCst), 4);

        // a restore under way refuses first, as before
        begin_restore_boundary_in(&marks).unwrap();
        let refused_for_the_restore = resume(Some("create-b")).await;
        assert!(refused_for_the_restore
            .result
            .is_err_and(|error| error.downcast_ref::<RestoreUnderWay>().is_some()));

        // the token as the one-shots read it
        let args = |list: &[&str]| list.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
        assert_eq!(
            boundary_token(&args(&["fc-agent", "--resume-snapshot-network"])).unwrap(),
            None
        );
        assert_eq!(
            boundary_token(&args(&[
                "fc-agent",
                "--prepare-snapshot-network",
                "--boundary",
                "a1-b2"
            ]))
            .unwrap(),
            Some("a1-b2")
        );
        for bad in [
            &["--boundary"][..],
            &["--boundary", ""],
            &["--boundary", "a b"],
            &["--boundary", "a\nagent-restarted"],
        ] {
            assert!(boundary_token(&args(bad)).is_err(), "{bad:?}");
        }
        // The check is under the lock every prepare marks under, and both refusals exit
        // with the status the host does not retry.
        let source = include_str!("snapshot_network.rs");
        let checks = &source[source.find("async fn resume_source_network_in<").unwrap()..];
        let checks = &checks[..checks.find("\n}\n").unwrap()];
        assert!(
            checks.find("acquire_boundary_lock_at(lock)").unwrap()
                < checks
                    .find("resume_is_for_the_owed_boundary(token)")
                    .unwrap()
        );
        let main: String = include_str!("main.rs").split_whitespace().collect();
        assert!(main.contains(
            "||error.downcast_ref::<snapshot_network::StaleResume>().is_some();std::process::exit(ifrefused{REFUSED_EXIT}else{1});"
        ));
    }

    /// The host fails a snapshot create on the restarted agent's report, which it finds
    /// by its text.
    #[test]
    fn the_host_reads_the_restarted_agents_report_as_the_guest_writes_it() {
        let host = include_str!("../../src/commands/common.rs").replace(char::is_whitespace, "");
        let expected =
            format!("const SOURCE_AGENT_RESTARTED_REPORT: &str = \"{AGENT_RESTARTED_REPORT}\";")
                .replace(char::is_whitespace, "");
        assert!(host.contains(&expected));
    }

    /// Any part of the gate, in either family, is a gate that is installed; a read that
    /// fails is an error, not "open".
    #[tokio::test]
    async fn a_gate_is_installed_while_any_of_its_chains_or_hooks_is() {
        struct Rules(&'static str, &'static str, bool);
        impl CommandRunner for Rules {
            async fn output(
                &self,
                program: &str,
                args: &[&str],
            ) -> io::Result<std::process::Output> {
                assert_eq!(args, ["-w", "-S"]);
                let rules = if program == "iptables" {
                    self.0
                } else {
                    self.1
                };
                Ok(std::process::Output {
                    status: std::process::ExitStatus::from_raw(if self.2 { 0 } else { 256 }),
                    stdout: rules.as_bytes().to_vec(),
                    stderr: Vec::new(),
                })
            }
            async fn output_with_input(
                &self,
                _program: &str,
                _args: &[&str],
                _input: &[u8],
            ) -> io::Result<std::process::Output> {
                unreachable!("a read of the gate writes nothing")
            }
        }
        let none = "-P INPUT ACCEPT\n-P OUTPUT ACCEPT\n";
        assert!(!gate_is_installed_with(&Rules(none, none, true))
            .await
            .unwrap());
        for part in ["-N FCVM_SNAPSHOT_IN\n", "-A OUTPUT -j FCVM_SNAPSHOT_OUT\n"] {
            assert!(gate_is_installed_with(&Rules(part, none, true))
                .await
                .unwrap());
            assert!(gate_is_installed_with(&Rules(none, part, true))
                .await
                .unwrap());
        }
        assert!(gate_is_installed_with(&Rules(none, none, false))
            .await
            .is_err());
        assert!(none.len() > 1 && !none.contains(GATE_INPUT_CHAIN));
    }

    /// The boundary lock is "held" only when someone holds it. A lock that cannot be
    /// opened is not held: the watchdog and the container's start wait on "held", and a
    /// `/run` that cannot be written must not make them wait for ever.
    ///
    /// RED BEFORE THE FIX: a lock that could not be opened read as held.
    #[test]
    fn a_boundary_lock_that_cannot_be_opened_is_not_held() {
        let dir = tempfile::tempdir().unwrap();
        let marks = marks_in(dir.path());
        let lock = dir.path().join("run").join("lock");
        assert!(matches!(try_boundary_lock_at(&lock), BoundaryLock::Free(_)));
        let held = acquire_boundary_lock_at(&lock).unwrap();
        assert!(matches!(try_boundary_lock_at(&lock), BoundaryLock::Held));
        assert!(boundary_in_progress_at(&marks, &lock));
        drop(held);
        // a directory where the lock file should be: it exists, and cannot be opened
        let unopenable = dir.path().join("run").join("a-directory");
        std::fs::create_dir_all(&unopenable).unwrap();
        assert!(matches!(
            try_boundary_lock_at(&unopenable),
            BoundaryLock::Unknown
        ));
        assert!(!boundary_in_progress_at(&marks, &unopenable));
    }

    /// Whose a closed gate is. A restore that is dispatched takes the boundary: its mark
    /// is written and the image's `owed`, which was its source's, goes. A boundary is in
    /// progress while the lock is held or a restore is under way.
    #[test]
    fn a_dispatched_restore_takes_the_boundary_from_the_image() {
        let dir = tempfile::tempdir().unwrap();
        let marks = marks_in(dir.path());
        let lock = dir.path().join("run").join("lock");
        assert!(!marks.source_owed() && !boundary_in_progress_at(&marks, &lock));

        // the image of a snapshot: its source's prepare left `owed`
        BoundaryMarks::mark(&marks.owed).unwrap();
        assert!(marks.source_owed());
        assert!(!boundary_in_progress_at(&marks, &lock));

        let held = acquire_boundary_lock_at(&lock).unwrap();
        assert!(
            boundary_in_progress_at(&marks, &lock),
            "a held lock is a boundary in progress"
        );
        drop(held);
        assert!(!boundary_in_progress_at(&marks, &lock));

        begin_restore_boundary_in(&marks).unwrap();
        assert!(marks.restore_under_way());
        assert!(!marks.source_owed(), "a clone owes its host no resume");
        assert!(boundary_in_progress_at(&marks, &lock));
        BoundaryMarks::clear(&marks.restore).unwrap();
        assert!(!boundary_in_progress_at(&marks, &lock));
        // twice is the same as once
        BoundaryMarks::clear(&marks.restore).unwrap();

        // The watcher marks the restore before anything else of it, and ends it at
        // publication, before the acknowledgement.
        let mmds = include_str!("mmds.rs");
        let apply = function_body(mmds, "async fn apply_restore_epoch(");
        let at = |needle: &str| {
            apply
                .find(needle)
                .unwrap_or_else(|| panic!("{needle} is not in apply_restore_epoch"))
        };
        let begin = at("crate::snapshot_network::begin_restore_boundary()");
        assert!(begin < at("crate::console::release_source_snapshot_hold("));
        assert!(begin < at("signals.restore_status.begin()"));
        let end = at("crate::snapshot_network::end_restore_boundary()");
        assert!(at("crate::restore::handle_clone_restore(") < end);
        assert!(end < at("crate::vsock::notify_restore_complete("));
        // The resume one-shot refuses before it waits for the lock, which a restore holds
        // until it publishes, and again under it.
        let source = include_str!("snapshot_network.rs");
        let resume = function_body(source, "async fn resume_source_network_in<");
        let locked = resume.find("acquire_boundary_lock_at(lock)").unwrap();
        let refusal = "RestoreUnderWay(\"a source's resume\")";
        assert!(resume.find(refusal).unwrap() < locked);
        assert!(resume.rfind(refusal).unwrap() > locked);
    }

    /// The barrier waits its grace period in its own thread. Releasing a
    /// descriptor waits only when it is the last reference: a process forked on
    /// another thread while the barrier's AF_PACKET socket was open held one
    /// until its exec, made the final release there, and the barrier returned
    /// without waiting (the same transfer moved the packet sender's release
    /// into `iptables -S`, 2-3 ms to 27-75 ms, #1151). `membarrier(2)`'s
    /// MEMBARRIER_CMD_GLOBAL is `synchronize_rcu()` in the caller.
    #[test]
    fn the_receive_barrier_waits_in_its_own_thread_not_on_a_release() {
        let source = include_str!("snapshot_network.rs");
        let start = source
            .find("impl SystemPacketReceiveBarrier {")
            .expect("the barrier's constructor");
        let constructor = &source[start..start + source[start..].find("\n}\n").unwrap()];
        assert!(
            constructor.contains("grace_period: membarrier_grace_period"),
            "the production barrier must wait with membarrier: {constructor}"
        );
        let start = source
            .find("fn membarrier_grace_period()")
            .expect("membarrier_grace_period");
        let body = &source[start..start + source[start..].find("\n}\n").unwrap()];
        assert!(
            body.contains("libc::SYS_membarrier") && body.contains("MEMBARRIER_CMD_GLOBAL"),
            "{body}"
        );
        assert!(
            !body.contains("open_raw_socket"),
            "the grace period must not be a descriptor release: {body}"
        );
        // The kernel this runs on accepts the command (it is refused under
        // nohz_full, and without CONFIG_MEMBARRIER).
        membarrier_grace_period().unwrap();
    }

    /// A gate template whose rules carry distinct tags per family and chain.
    fn model_template() -> GateTemplate {
        use crate::nftables::model::body;
        let family = |base: u8| FamilyTemplate {
            input: (0..4).map(|i| body(base + i, 0)).collect(),
            output: (0..4).map(|i| body(base + 10 + i, 0)).collect(),
            udp: body(base + 20, 0),
        };
        GateTemplate {
            families: [family(10), family(110)],
        }
    }

    /// The armed gate of both families as the template writes it, with
    /// counters that traffic has moved since.
    fn template_armed_model(template: &GateTemplate) -> Arc<Mutex<crate::nftables::model::Kernel>> {
        let mut kernel = crate::nftables::model::Kernel::default();
        for family in Family::BOTH {
            let wanted = &template.families[family_index(family)];
            let moved = |rules: &[crate::nftables::RuleBody]| -> Vec<crate::nftables::RuleBody> {
                rules
                    .iter()
                    .enumerate()
                    .map(|(i, rule)| {
                        let tag = rule.expressions[rule.expressions.len() - 4];
                        crate::nftables::model::body(tag, 1000 + i as u64)
                    })
                    .collect()
            };
            kernel.arm_with(family, &moved(&wanted.input), &moved(&wanted.output));
        }
        Arc::new(Mutex::new(kernel))
    }

    fn chain_bodies(
        kernel: &crate::nftables::model::Kernel,
        family: Family,
        chain: &str,
    ) -> Vec<crate::nftables::model::Rule> {
        let table = match family {
            Family::Ipv4 => &kernel.ipv4,
            Family::Ipv6 => &kernel.ipv6,
        };
        table
            .rules
            .iter()
            .filter(|(c, _, _)| *c == chain)
            .map(|(_, _, rule)| rule.clone())
            .collect()
    }

    /// A restored clone verifies its inherited gate and adds the UDP rule over
    /// the nfnetlink socket it keeps, and starts no process for either: the
    /// four iptables processes they took were 8 to 12 ms of a 17 to 24 ms
    /// restore on m8g.metal-24xl. The UDP rule lands in both families in one
    /// commit, below the two loopback exemptions, and the gate still opens.
    ///
    /// RED BEFORE THE CHANGE: verification ran `iptables -S` per family, so
    /// with a runner that answers nothing it read an empty table and reported
    /// the gate open.
    #[tokio::test]
    async fn a_verified_restore_gates_udp_without_starting_a_process() {
        let template = Arc::new(model_template());
        let kernel = template_armed_model(&template);
        let runner = RecordingRunner::default();
        let calls = runner.0.clone();
        let gate = IptablesGate::with_netfilter(runner, ModelNetfilter::new(kernel.clone()))
            .with_template(Some(template.clone()));

        assert!(
            gate.is_closed().await.unwrap(),
            "the armed gate did not verify"
        );
        gate.gate_udp_ingress().await.unwrap();
        assert_eq!(
            calls.lock().unwrap().clone(),
            Vec::<String>::new(),
            "verification and the UDP rule started processes"
        );
        {
            let k = kernel.lock().unwrap();
            assert_eq!(k.commits.len(), 1, "{:?}", k.commits);
            let ops = k.commits[0].join("; ");
            assert!(
                ops.contains("ip insert rule FCVM_SNAPSHOT_IN")
                    && ops.contains("ip6 insert rule FCVM_SNAPSHOT_IN"),
                "{ops}"
            );
            for family in Family::BOTH {
                let wanted = &template.families[family_index(family)];
                let rules = chain_bodies(&k, family, GATE_INPUT_CHAIN);
                assert_eq!(rules.len(), 5, "{} {rules:?}", family.name());
                assert_eq!(
                    rules[2],
                    crate::nftables::model::Rule::Body(wanted.udp.clone()),
                    "{}: the UDP rule is not third",
                    family.name()
                );
            }
        }
        gate.open_egress().await.unwrap();
        gate.open().await.unwrap();
        let k = kernel.lock().unwrap();
        for table in [&k.ipv4, &k.ipv6] {
            assert!(!holds_gate(table, GATE_INPUT_CHAIN), "{table:?}");
            assert!(!holds_gate(table, GATE_OUTPUT_CHAIN), "{table:?}");
        }
    }

    /// Verification over nfnetlink refuses what verification over iptables
    /// refuses: a changed rule, an extra rule, a missing chain, and a rule
    /// ahead of the hook. Counters are not part of a rule.
    #[tokio::test]
    async fn netlink_verification_refuses_a_gate_that_is_not_the_template() {
        use crate::nftables::model::{body, Rule};
        let template = Arc::new(model_template());
        type Change = Box<dyn Fn(&mut crate::nftables::model::Kernel)>;
        let cases: Vec<(&str, Change)> = vec![
            ("as armed", Box::new(|_| {})),
            (
                "a changed rule",
                Box::new(|k| {
                    let slot = k
                        .ipv6
                        .rules
                        .iter_mut()
                        .find(|(c, _, _)| *c == GATE_OUTPUT_CHAIN)
                        .unwrap();
                    slot.2 = Rule::Body(body(99, 0));
                }),
            ),
            (
                "an extra rule",
                Box::new(|k| {
                    k.add_rule(Family::Ipv4, "FCVM_SNAPSHOT_IN", Rule::Body(body(98, 0)));
                }),
            ),
            (
                "a missing chain",
                Box::new(|k| {
                    k.ipv4.rules.retain(|(c, _, _)| *c != GATE_OUTPUT_CHAIN);
                    k.ipv4.chains.retain(|c| *c != GATE_OUTPUT_CHAIN);
                }),
            ),
            (
                "a rule ahead of the hook",
                Box::new(|k| {
                    k.ipv4.rules.insert(0, ("INPUT", 9999, Rule::Other));
                }),
            ),
        ];
        for (case, change) in cases {
            let kernel = template_armed_model(&template);
            change(&mut kernel.lock().unwrap());
            let gate = IptablesGate::with_netfilter(
                RecordingRunner::default(),
                ModelNetfilter::new(kernel),
            )
            .with_template(Some(template.clone()));
            assert_eq!(
                gate.is_closed().await.unwrap(),
                case == "as armed",
                "{case}"
            );
        }
    }

    /// The UDP rule's batch is pinned to the generation its handles were read
    /// under. A commit that lands in between refuses it whole, and it is read
    /// and sent again: one UDP rule per family, never two.
    #[tokio::test]
    async fn the_udp_rule_is_read_and_sent_again_when_the_ruleset_moves() {
        let template = Arc::new(model_template());
        let kernel = template_armed_model(&template);
        kernel.lock().unwrap().before_batch = Some(Box::new(|k| k.genid += 1));
        let gate = IptablesGate::with_netfilter(
            RecordingRunner::default(),
            ModelNetfilter::new(kernel.clone()),
        )
        .with_template(Some(template.clone()));
        gate.gate_udp_ingress().await.unwrap();
        let k = kernel.lock().unwrap();
        assert_eq!(k.refused, 1);
        assert_eq!(k.commits.len(), 1, "{:?}", k.commits);
        for family in Family::BOTH {
            let udp = crate::nftables::model::Rule::Body(
                template.families[family_index(family)].udp.clone(),
            );
            let rules = chain_bodies(&k, family, GATE_INPUT_CHAIN);
            assert_eq!(rules.iter().filter(|r| **r == udp).count(), 1, "{rules:?}");
        }
    }

    /// The template is used only when iptables renders a gate with the
    /// inserted UDP rule exactly as `udp_ingress_rule_args` spells it, third in
    /// the INPUT gate chain.
    #[test]
    fn the_template_is_used_only_when_iptables_renders_the_inserted_rule() {
        let rendered = |udp: &str| {
            let mut dump = String::from("-P INPUT ACCEPT\n-N FCVM_SNAPSHOT_IN\n");
            for (index, rule) in gate_chain_rules("-s", IPV4_LOOPBACK).iter().enumerate() {
                if index == 2 {
                    dump.push_str(&format!("-A FCVM_SNAPSHOT_IN {udp}\n"));
                }
                dump.push_str(&format!("-A FCVM_SNAPSHOT_IN {}\n", rule.join(" ")));
            }
            dump
        };
        let written = udp_ingress_rule_args(IPV4_LOOPBACK).join(" ");
        assert!(scratch_rendering_matches(
            &rendered(&written),
            IPV4_LOOPBACK
        ));
        assert!(!scratch_rendering_matches(
            &rendered("-p udp -m conntrack --ctdir ORIGINAL -j DROP"),
            IPV4_LOOPBACK
        ));
        assert!(!scratch_rendering_matches(
            "-P INPUT ACCEPT\n-N FCVM_SNAPSHOT_IN\n",
            IPV4_LOOPBACK
        ));
    }
}
