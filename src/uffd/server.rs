use anyhow::{anyhow, Context, Result};
use std::fs::File;
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::unix::AsyncFd;
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinSet;
use tracing::{debug, error, info, warn};

use memmap2::MmapOptions;
use userfaultfd::{Event, FaultKind, Uffd};
use vmm_sys_util::sock_ctrl_msg::ScmSocket;

/// 2MiB — the only huge page size fcvm supports for guest memory.
const HUGE_PAGE_2M: usize = 2 * 1024 * 1024;

/// Env var overriding [`DEFAULT_MAX_CLONES_PER_SERVER`].
pub const MAX_CLONES_ENV: &str = "FCVM_UFFD_MAX_CLONES";

/// The largest per-server clone fan-out the test suite exercises
/// (`test_snapshot_clone_stress_100_*` restores 100 clones from ONE serve process). The
/// default cap must stay above it: a bound that refuses a configuration fcvm supports is a
/// bug, not a safety feature.
const EXERCISED_MAX_CLONES_PER_SERVER: usize = 100;

/// How many clones one server serves concurrently.
///
/// Every clone attached to a server shares that server's **failure domain** (one process,
/// one page source, one set of fds) and its **fairness domain** (one task each on the same
/// runtime). An unbounded server therefore has an unbounded blast radius: a single
/// fault-handler failure, OOM, or fd exhaustion takes out every clone attached to it.
///
/// This is a **backstop, not a ration**. It sits well clear of
/// [`EXERCISED_MAX_CLONES_PER_SERVER`] so it never fires in supported use — the first
/// version of this change shipped 64 and the 100-clone stress test caught it immediately,
/// which is exactly the failure mode a too-tight bound produces. Raise it with
/// `FCVM_UFFD_MAX_CLONES` and accept the wider blast radius, or run a second
/// `fcvm snapshot serve` and split the clones across two failure domains.
pub const DEFAULT_MAX_CLONES_PER_SERVER: usize = 256;

// Lowering the default back under the fan-out fcvm actually supports is a BUILD error, not a
// mystery stress-test failure two hours later. (Shipping 64 cost exactly that.)
const _: () = assert!(
    DEFAULT_MAX_CLONES_PER_SERVER > EXERCISED_MAX_CLONES_PER_SERVER,
    "the default UFFD clone cap must exceed the per-server fan-out the stress tests restore"
);

/// How many uffd events one handler drains before yielding to the runtime.
///
/// `drain_events` is a synchronous loop that owns its worker thread while it runs, and it
/// drains until the queue is EMPTY. A clone faulting hard enough to keep refilling its queue
/// would hold that thread indefinitely and starve every other clone on the server — one
/// clone's page-in storm becomes every clone's stall. Batching bounds one handler's
/// uninterrupted turn without building a scheduler: after each batch the handler yields and
/// the runtime picks whoever has waited longest.
///
/// A yield costs a queue push, not a syscall, so even at a million faults/second the batching
/// overhead is noise — while the clones waiting behind it are vCPUs stopped dead on a fault.
const MAX_EVENTS_PER_BATCH: usize = 128;

/// How long a connecting VMM has to complete the UFFD handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// `sockaddr_un.sun_path` is a fixed 108-byte array on Linux; bind(2) fails with a bare
/// `EINVAL` when a path overflows it. Checked up front so the error names the real problem.
const MAX_UNIX_SOCKET_PATH_LEN: usize = 107;

/// Resolve the per-server clone cap from an explicit [`MAX_CLONES_ENV`] value.
///
/// A malformed or zero value is an error, not a silent fallback: a server that quietly
/// ignores its configured bound is exactly the unbounded server the cap exists to prevent.
fn parse_max_clones(raw: Option<&str>) -> Result<usize> {
    let Some(raw) = raw else {
        return Ok(DEFAULT_MAX_CLONES_PER_SERVER);
    };
    let parsed: usize = raw
        .trim()
        .parse()
        .with_context(|| format!("{MAX_CLONES_ENV}={raw:?} is not a non-negative integer"))?;
    anyhow::ensure!(
        parsed > 0,
        "{MAX_CLONES_ENV}=0 would refuse every clone; set it to at least 1"
    );
    Ok(parsed)
}

/// Resolve the per-server clone cap from the environment.
fn max_clones_per_server() -> Result<usize> {
    parse_max_clones(std::env::var(MAX_CLONES_ENV).ok().as_deref())
}

/// `SO_PEERPIDFD` (Linux 6.5+). Not exposed by the `libc` crate; value from
/// `asm-generic/socket.h`.
const SO_PEERPIDFD: libc::c_int = 77;

/// Fetch a pidfd for the peer of a connected Unix socket, atomically (see [`PeerVmm`]).
fn peer_pidfd(sock: RawFd) -> Result<OwnedFd> {
    let mut raw: libc::c_int = -1;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: getsockopt into a `c_int` with its exact size; `sock` is a live socket fd.
    let rc = unsafe {
        libc::getsockopt(
            sock,
            libc::SOL_SOCKET,
            SO_PEERPIDFD,
            std::ptr::addr_of_mut!(raw).cast::<libc::c_void>(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).context("getsockopt(SO_PEERPIDFD)");
    }
    // SAFETY: the kernel just installed `raw` as a fresh, owned fd in this process.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// The PID a pidfd refers to, from `/proc/self/fdinfo/<fd>`. Log-only (see [`PeerVmm::pid`]).
fn pidfd_pid(pidfd: &OwnedFd) -> Result<u32> {
    let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{}", pidfd.as_raw_fd()))
        .context("reading pidfd fdinfo")?;
    info.lines()
        .find_map(|l| l.strip_prefix("Pid:")?.trim().parse().ok())
        .ok_or_else(|| anyhow!("pidfd fdinfo has no parsable Pid: line"))
}

/// Refuse to start unless this kernel can pin a socket peer atomically.
///
/// Checked ONCE, at server construction, so the unsupported-kernel case is a loud startup
/// error instead of a per-connection surprise on a server that is already serving clones.
/// Without `SO_PEERPIDFD` there is no race-free way to identify the VMM behind a connection,
/// and without that there is no way to guarantee it can be stopped — which is the entire
/// premise of this server (see [`PeerVmm`]).
fn require_peer_pidfd_support() -> Result<()> {
    let (a, _b) = std::os::unix::net::UnixStream::pair().context("probing SO_PEERPIDFD")?;
    peer_pidfd(a.as_raw_fd()).map(|_| ()).context(
        "this kernel cannot report a peer pidfd (SO_PEERPIDFD, Linux 6.5+), so the UFFD \
         memory server cannot guarantee it is able to stop a clone whose page faults it \
         fails to serve — refusing to start rather than serving memory it cannot fail \
         closed on",
    )
}

/// Last-resort stop for a peer we could not pin: SIGKILL the PID `SO_PEERCRED` reports.
///
/// Only reachable when [`peer_pidfd`] fails on a kernel that passed the startup probe, i.e.
/// file-descriptor exhaustion. `SO_PEERCRED` needs no fd, so it still works there. This is
/// the one place in the server that signals a bare PID; it is justified only because the
/// alternative (drop the connection) releases a possibly in-flight userfaultfd and corrupts
/// that guest with certainty.
fn kill_unpinned_peer(stream: &UnixStream, vm_id: &str) {
    let Ok(cred) = stream.peer_cred() else {
        error!(
            target: "uffd",
            vm_id = %vm_id,
            "peer could be neither pinned nor identified — a VMM may now be running on \
             unserved memory and this server cannot stop it"
        );
        return;
    };
    let Some(pid) = cred.pid() else {
        error!(target: "uffd", vm_id = %vm_id, "peer credentials carry no PID; cannot stop the VMM");
        return;
    };
    let killed = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid),
        nix::sys::signal::Signal::SIGKILL,
    );
    error!(
        target: "uffd",
        vm_id = %vm_id,
        peer_pid = pid,
        result = ?killed,
        "killed an UNPINNED peer VMM (could not obtain its pidfd)"
    );
}

/// The VMM process on the other end of one clone's UFFD connection, held as a **pidfd** so
/// signalling it can never land on a different process.
///
/// This is what makes the server fail CLOSED. A clone's guest memory exists only as long as
/// this server answers its faults: once Firecracker hands over the userfaultfd it closes its
/// own copy, so if this server stops resolving faults the kernel resolves them with ZERO
/// PAGES and the guest keeps running on silently corrupt memory. There is no in-band way to
/// tell that guest "your memory is gone". The only honest response is to stop the VMM, and
/// the only safe way to stop it is a handle that refers to the process itself — `SO_PEERCRED`
/// gives the PID at connect time, `pidfd_open` pins it, and `pidfd_send_signal` delivers to
/// that exact process even if the PID has since been recycled.
#[derive(Debug)]
struct PeerVmm {
    /// The peer's PID, read back from the pidfd itself — for LOGS ONLY. Every decision uses
    /// the pidfd, so a PID that has since been recycled cannot misdirect anything.
    pid: u32,
    pidfd: OwnedFd,
}

impl PeerVmm {
    /// Pin the process that opened `stream`, atomically.
    ///
    /// `SO_PEERPIDFD` hands back a **pidfd** for the peer that was recorded at connect time.
    /// The obvious-looking alternative — `SO_PEERCRED` for a PID, then `pidfd_open(pid)` — has
    /// a real window: if the peer exits and is reaped between the two calls and the kernel
    /// recycles its PID, the pidfd pins a STRANGER, and every later decision (including a
    /// SIGKILL) lands on that stranger. There is no amount of re-checking that closes the
    /// window, because the check itself is a second observation of the same racy number. So
    /// the identity is taken from the socket in one atomic step and never re-derived.
    fn from_stream(stream: &UnixStream) -> Result<Self> {
        let pidfd = peer_pidfd(stream.as_raw_fd())
            .context("SO_PEERPIDFD on the accepted UFFD connection")?;
        // Read the PID back OUT of the pinned handle rather than from SO_PEERCRED, so even
        // the log line cannot name a different process than the one we hold.
        let pid = pidfd_pid(&pidfd).context("reading the peer pidfd's PID from fdinfo")?;
        Ok(Self { pid, pidfd })
    }

    /// Pin an already-known PID via `pidfd_open`. Test-only: production identity always comes
    /// from the socket ([`PeerVmm::from_stream`]), which has no PID-reuse window.
    #[cfg(test)]
    fn from_pid(pid: u32) -> Result<Self> {
        // SAFETY: pidfd_open(2) with no flags; the returned fd is owned by us.
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
        if raw < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("pidfd_open on PID {pid}"));
        }
        // SAFETY: `raw` is a fresh, owned, valid file descriptor.
        let pidfd = unsafe { OwnedFd::from_raw_fd(raw as RawFd) };
        Ok(Self { pid, pidfd })
    }

    /// Whether the pinned process is still running — asked of the pidfd (signal 0), not of
    /// its PID, so a recycled PID can never make a dead VMM look alive.
    fn is_alive(&self) -> bool {
        // SAFETY: pidfd_send_signal(2) with signal 0 (existence probe) on an owned pidfd.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.pidfd.as_raw_fd(),
                0,
                std::ptr::null_mut::<libc::siginfo_t>(),
                0,
            )
        };
        rc == 0
    }

    /// SIGKILL the VMM. Returns whether the signal was delivered.
    ///
    /// SIGKILL, not SIGTERM: a VMM whose memory this server can no longer serve must not get
    /// to run *any* more guest instructions, and a graceful shutdown would let the guest keep
    /// executing (on zero pages) while it drains.
    fn kill_now(&self, reason: &str) -> bool {
        // SAFETY: pidfd_send_signal(2) on an owned pidfd; no siginfo override, no flags.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.pidfd.as_raw_fd(),
                libc::SIGKILL,
                std::ptr::null_mut::<libc::siginfo_t>(),
                0,
            )
        };
        if rc == 0 {
            error!(
                target: "uffd",
                peer_pid = self.pid,
                reason = %reason,
                "KILLED the clone's VMM: its guest memory can no longer be served, and a \
                 surviving VMM would run on zero-filled pages (silent corruption). The clone \
                 is now dead and its `fcvm snapshot run` exits non-zero."
            );
            return true;
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            info!(
                target: "uffd",
                peer_pid = self.pid,
                reason = %reason,
                "clone's VMM had already exited; nothing to fail closed on"
            );
            return false;
        }
        error!(
            target: "uffd",
            peer_pid = self.pid,
            reason = %reason,
            error = %err,
            "COULD NOT kill the clone's VMM — it may still be running on unserved memory"
        );
        false
    }
}

/// How the server materialises snapshot pages into a clone's guest memory.
///
/// Both modes serve the same snapshot; they differ in whether the physical page a clone
/// reads is *shared* with the other clones.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UffdBacking {
    /// MISSING faults on anonymous guest memory, resolved with `UFFDIO_COPY` out of a
    /// mapping of the snapshot memory file.
    ///
    /// Lazy, but every clone ends up with its own private copy of every page it touches:
    /// N clones that touch the same page cost N physical pages.
    Copy,
    /// MINOR faults on a `MAP_PRIVATE` mapping of a shared memfd, resolved with
    /// `UFFDIO_CONTINUE`.
    ///
    /// The server holds the snapshot's guest memory in one memfd and hands that fd to every
    /// clone's Firecracker over the handshake socket. Because the clone maps it
    /// `MAP_PRIVATE`, `UFFDIO_CONTINUE` installs a **read-only** PTE pointing at the shared
    /// page-cache folio (`mm/userfaultfd.c`: `if (page_in_cache && !vm_shared) writable =
    /// false`), so reads across all clones hit ONE physical copy and the first guest write
    /// takes an ordinary copy-on-write fault into private memory. The snapshot stays
    /// pristine and reusable.
    ///
    /// `hugepages` selects `MFD_HUGETLB|MFD_HUGE_2MB` for 2MiB-backed guests.
    Minor { hugepages: bool },
}

impl UffdBacking {
    /// Parse the `FCVM_UFFD_MODE` env var / `--uffd-mode` flag value.
    pub fn parse_mode(value: &str, hugepages: bool) -> Result<Self> {
        match value {
            "copy" => Ok(Self::Copy),
            "minor" => Ok(Self::Minor { hugepages }),
            other => anyhow::bail!("invalid UFFD mode {other:?} (expected \"copy\" or \"minor\")"),
        }
    }

    /// Resolve the backing mode from `FCVM_UFFD_MODE`, defaulting to [`UffdBacking::Copy`].
    pub fn from_env(hugepages: bool) -> Result<Self> {
        match std::env::var("FCVM_UFFD_MODE") {
            Ok(v) => Self::parse_mode(&v, hugepages),
            Err(_) => Ok(Self::Copy),
        }
    }

    /// The Firecracker `mem_backend.backend_type` string this mode requires.
    pub fn firecracker_backend_type(&self) -> &'static str {
        match self {
            Self::Copy => "Uffd",
            Self::Minor { .. } => "UffdMinor",
        }
    }

    /// Short name used in state files and logs.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Copy => "copy",
            Self::Minor { .. } => "minor",
        }
    }
}

/// Where a clone's pages come from — the per-mode server state, shared by all clones.
enum PageSource {
    /// Read-only mapping of the snapshot memory file; pages are `UFFDIO_COPY`'d out of it.
    Copy { mmap: memmap2::Mmap },
    /// A memfd holding the whole snapshot image. Handed to each Firecracker over
    /// `SCM_RIGHTS`; faults are resolved in place with `UFFDIO_CONTINUE`.
    Minor { backing: File },
}

/// Async UFFD server that serves memory pages for multiple VMs from a single snapshot
pub struct UffdServer {
    snapshot_id: String,
    socket_path: PathBuf,
    source: Arc<PageSource>,
    backing: UffdBacking,
    max_clones: usize,
}

impl UffdServer {
    /// The socket path the server run by `(pid, pid_start_time)` binds inside `dir`.
    ///
    /// The server-instance identity is IN the path. It used to be derived from the snapshot
    /// name (and later the PID) alone, which meant two servers could name the same socket —
    /// and since a server unlinks that path both before binding and again on drop, a second
    /// server would delete the socket a live first one was still accepting on, or delete the
    /// socket a live *second* one had just bound. Clones then connected to nothing.
    ///
    /// `(pid, pid_start_time)` is fcvm's existing process identity (`VmState::pid_start_time`,
    /// verified by `load_state_by_pid`), so no two live servers can ever produce the same
    /// name, the unlink-before-bind can only remove this same process's own leftovers, and
    /// the unlink-on-drop can only remove our own socket. It is also reconstructible: a clone
    /// reads the serve process's `pid` + `pid_start_time` out of its state file and derives
    /// exactly this path — no extra state, no lookup by scanning.
    pub fn socket_path_for(dir: &Path, name: &str, pid: u32, pid_start_time: u64) -> PathBuf {
        dir.join(format!("uffd-{name}-{pid}-{pid_start_time}.sock"))
    }

    /// Create a UFFD server that binds its own unique socket inside `dir`.
    pub async fn new(
        snapshot_id: String,
        mem_file_path: &Path,
        dir: &Path,
        backing: UffdBacking,
    ) -> Result<Self> {
        // Before anything else: prove we can fail closed on this kernel.
        require_peer_pidfd_support()?;

        let my_pid = std::process::id();
        let my_start_time = crate::utils::process_start_time(my_pid)
            .ok_or_else(|| anyhow!("cannot read this process's own start time from /proc"))?;
        let socket_path = Self::socket_path_for(dir, &snapshot_id, my_pid, my_start_time);

        let path_len = socket_path.as_os_str().len();
        anyhow::ensure!(
            path_len <= MAX_UNIX_SOCKET_PATH_LEN,
            "UFFD socket path is {path_len} bytes, over the {MAX_UNIX_SOCKET_PATH_LEN}-byte \
             sun_path limit: {} — use a shorter snapshot tag or data dir",
            socket_path.display()
        );

        info!(
            target: "uffd",
            snapshot = %snapshot_id,
            mem_file = %mem_file_path.display(),
            socket = %socket_path.display(),
            mode = backing.name(),
            "creating UFFD server"
        );
        let socket_path = &socket_path;

        // Open the memory snapshot file (shared across all VMs)
        let mem_file = File::open(mem_file_path).context("opening memory file")?;
        let mem_size = mem_file.metadata()?.len() as usize;

        info!(
            target: "uffd",
            mem_size_mb = mem_size / (1024 * 1024),
            mode = backing.name(),
            "preparing snapshot page source"
        );

        let source = match backing {
            UffdBacking::Copy => {
                // Safety: We're mapping a read-only file for serving pages
                let mmap = unsafe {
                    MmapOptions::new()
                        .len(mem_size)
                        .map(&mem_file)
                        .context("mmapping memory file")?
                };
                PageSource::Copy { mmap }
            }
            UffdBacking::Minor { hugepages } => {
                let backing_file = tokio::task::spawn_blocking({
                    let id = snapshot_id.clone();
                    move || create_backing_memfd(&id, mem_file, mem_size, hugepages)
                })
                .await
                .context("joining memfd population task")??;
                PageSource::Minor {
                    backing: backing_file,
                }
            }
        };

        // Ensure parent directory exists
        if let Some(parent) = socket_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .context("creating socket directory")?;
        }

        // Remove leftovers at OUR path. Safe to do blindly only because the path carries
        // this process's (pid, start_time): no live server can own this name, so the only
        // thing that can be here is a file this exact identity left behind (possible across
        // a reboot, which restarts the clock-tick counter). Ignore errors — nothing to
        // remove is the normal case.
        let _ = tokio::fs::remove_file(&socket_path).await;

        Ok(Self {
            snapshot_id,
            socket_path: socket_path.to_path_buf(),
            source: Arc::new(source),
            backing,
            max_clones: max_clones_per_server()?,
        })
    }

    /// Get the socket path for this server
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// The page-materialisation mode this server was created with.
    pub fn backing(&self) -> UffdBacking {
        self.backing
    }

    /// Run the UFFD server (blocks until cancelled via CancellationToken)
    pub async fn run(&self, cancel: tokio_util::sync::CancellationToken) -> Result<()> {
        info!(
            target: "uffd",
            snapshot = %self.snapshot_id,
            socket = %self.socket_path.display(),
            "starting UFFD server"
        );

        // Bind Unix socket
        let listener = UnixListener::bind(&self.socket_path).context("binding Unix socket")?;

        info!(target: "uffd", "UFFD server listening, waiting for VM connections...");

        let mut vm_tasks: JoinSet<String> = JoinSet::new();
        let mut next_vm_id = 0u64;
        // Admitted-clone count. Incremented only here (single accept loop) and decremented by
        // each connection task as it finishes, so it tracks clones that are actually being
        // served rather than tasks the JoinSet has yet to reap.
        let admitted = Arc::new(AtomicUsize::new(0));

        loop {
            tokio::select! {
                // Accept new VM connections
                accept_result = listener.accept() => {
                    match accept_result {
                        Ok((stream, _)) => {
                            let vm_id = format!("vm-{}", next_vm_id);
                            next_vm_id += 1;

                            // Pin the VMM on the other end BEFORE anything else: every
                            // failure from here on has to be able to stop it (see PeerVmm).
                            let peer = match PeerVmm::from_stream(&stream) {
                                Ok(peer) => peer,
                                Err(e) => {
                                    error!(
                                        target: "uffd",
                                        vm_id = %vm_id,
                                        error = ?e,
                                        "could not pin the connecting VMM — stopping it unpinned \
                                         rather than dropping the connection"
                                    );
                                    // Dropping the stream here would NOT be safe: Firecracker
                                    // may already have sent its userfaultfd and closed its own
                                    // copy, so closing the socket destroys the in-flight fd and
                                    // the guest runs on zero pages — the same trap the over-cap
                                    // path avoids. SO_PEERPIDFD is probed at startup, so the only
                                    // way to reach this is fd exhaustion mid-flight; the unpinned
                                    // PID from SO_PEERCRED is then the only handle left, and a
                                    // vanishingly unlikely mis-signal beats a CERTAIN corruption.
                                    kill_unpinned_peer(&stream, &vm_id);
                                    continue;
                                }
                            };

                            let in_flight = admitted.load(Ordering::Acquire);
                            if in_flight >= self.max_clones {
                                error!(
                                    target: "uffd",
                                    vm_id = %vm_id,
                                    peer_pid = peer.pid,
                                    active_clones = in_flight,
                                    max_clones = self.max_clones,
                                    "REFUSING clone: this server is at its concurrent-clone cap. \
                                     Start another `fcvm snapshot serve` (a second failure domain) \
                                     or raise {}.",
                                    MAX_CLONES_ENV
                                );
                                // Closing the socket is not enough to refuse safely: in COPY
                                // mode Firecracker sends its uffd and closes its own copy, so a
                                // connection dropped after that point releases the uffd and the
                                // guest silently runs on zero pages. Refusal means killing.
                                peer.kill_now("server is at its concurrent-clone cap");
                                continue;
                            }
                            admitted.fetch_add(1, Ordering::AcqRel);

                            let source = Arc::clone(&self.source);
                            let admitted_slot = Arc::clone(&admitted);

                            info!(
                                target: "uffd",
                                vm_id = %vm_id,
                                peer_pid = peer.pid,
                                active_clones = in_flight + 1,
                                max_clones = self.max_clones,
                                "new VM connection"
                            );

                            // Spawn per-connection task so the accept loop returns
                            // immediately — no blocking on slow/misbehaving clones.
                            vm_tasks.spawn(async move {
                                serve_clone_fail_closed(&vm_id, stream, source, peer).await;
                                admitted_slot.fetch_sub(1, Ordering::AcqRel);
                                vm_id
                            });
                        }
                        Err(e) => {
                            error!(target: "uffd", error = %e, "failed to accept connection");
                        }
                    }
                }

                // Handle completed VM tasks
                Some(result) = vm_tasks.join_next() => {
                    match result {
                        Ok(vm_id) => info!(target: "uffd", vm_id = %vm_id, "VM disconnected"),
                        Err(e) => error!(target: "uffd", error = %e, "VM task panicked"),
                    }

                    info!(target: "uffd", active_vms = vm_tasks.len(), "VM exited");
                }

                // Shut down when cancellation token is triggered (Ctrl-C / SIGTERM)
                _ = cancel.cancelled() => {
                    info!(target: "uffd", "cancellation requested, shutting down server");
                    break;
                }
            }
        }

        // Stop accepting new connections, but keep serving page faults for VMs that are
        // already connected until each one closes its uffd (i.e. its Firecracker exits).
        // Aborting the handlers here would close the uffds while those VMs are still
        // running, and the kernel would then resolve their page faults with zero pages
        // instead of snapshot contents — silent guest memory corruption.
        drop(listener);
        if !vm_tasks.is_empty() {
            info!(
                target: "uffd",
                active_vms = vm_tasks.len(),
                "draining VM handlers before shutdown"
            );
        }
        while let Some(result) = vm_tasks.join_next().await {
            match result {
                Ok(vm_id) => info!(target: "uffd", vm_id = %vm_id, "VM disconnected"),
                Err(e) => error!(target: "uffd", error = %e, "VM task panicked"),
            }
        }

        info!(target: "uffd", "UFFD server stopped");
        Ok(())
    }
}

impl Drop for UffdServer {
    fn drop(&mut self) {
        // Safe to remove unconditionally: `socket_path_for` puts this process's
        // (pid, start_time) in the name, so this path can only ever be ours — dropping one
        // server can no longer unlink the socket another live server is accepting on.
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

/// Serve one admitted clone, and KILL its VMM if serving fails at any point.
///
/// This is the whole fail-closed contract in one place. Before it, a handler error was only
/// logged: the clone's userfaultfd closed with the guest still running, the kernel began
/// resolving that guest's faults with zero pages, and the only trace was a log line asking a
/// human to "kill the affected clone". A request service cannot depend on someone reading a
/// log — the corrupted guest happily answers requests with garbage. Now the failure is
/// converted into the one state a caller cannot miss: a dead VMM, which makes the clone's
/// `fcvm snapshot run` exit non-zero (see `vmm_exit_failure` in `commands/snapshot.rs`).
async fn serve_clone_fail_closed(
    vm_id: &str,
    stream: UnixStream,
    source: Arc<PageSource>,
    peer: PeerVmm,
) {
    match serve_clone(vm_id, stream, source).await {
        Ok(()) => info!(
            target: "uffd",
            vm_id = %vm_id,
            peer_pid = peer.pid,
            "VM handler exited cleanly"
        ),
        Err(e) => {
            error!(
                target: "uffd",
                vm_id = %vm_id,
                peer_pid = peer.pid,
                peer_alive = peer.is_alive(),
                error = ?e,
                "clone's UFFD service FAILED — killing its VMM"
            );
            peer.kill_now(&format!("{e:#}"));
        }
    }
}

/// Serve one clone end to end: handshake, then page faults until its VMM exits.
///
/// Every error path out of this function is a fail-closed event for that clone — see the
/// caller in [`UffdServer::run`], which kills the VMM. Two failure classes exist and both
/// have the same remedy: the handshake can fail with the clone's userfaultfd already in
/// flight (COPY mode sends the fd and the mappings in ONE message, so a malformed message
/// means we hold an fd we cannot use), and the fault handler can fail after serving for
/// hours. In both cases the uffd ends up closed while the guest is alive, which the kernel
/// turns into zero-filled pages.
async fn serve_clone(vm_id: &str, stream: UnixStream, source: Arc<PageSource>) -> Result<()> {
    let (uffd, mappings) = tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake(stream, &source))
        .await
        .map_err(|_| anyhow!("UFFD handshake did not complete within {HANDSHAKE_TIMEOUT:?}"))?
        .context("UFFD handshake failed")?;

    info!(
        target: "uffd",
        vm_id = %vm_id,
        regions = mappings.len(),
        "received UFFD with {} memory regions",
        mappings.len()
    );

    handle_vm_page_faults(vm_id.to_string(), uffd, mappings, source).await
}

/// Whether a UFFDIO_ZEROPAGE error means the range (or part of it) is already populated.
///
/// Remove (balloon) events and page faults are not ordered, so a fault served with UFFDIO_COPY
/// before the Remove event is processed leaves pages present in the removed range. The kernel
/// then returns EEXIST (first page already present) or EAGAIN (range partially zeroed before
/// hitting a present page) from UFFDIO_ZEROPAGE. Neither is fatal — the pages are populated.
fn zeropage_hit_present_page(e: &userfaultfd::Error) -> bool {
    matches!(
        e,
        userfaultfd::Error::ZeropageFailed(errno)
            if (*errno as i32) == libc::EEXIST || (*errno as i32) == libc::EAGAIN
    )
}

/// Whether a UFFDIO_CONTINUE error means the page is already mapped in the clone.
///
/// Two threads in the guest can fault on the same page before the first `UFFDIO_CONTINUE`
/// lands; the kernel reports `EEXIST` for the loser. Nothing is wrong — the page is present.
fn continue_hit_present_page(e: &userfaultfd::Error) -> bool {
    // Compare the raw errno: the `userfaultfd` crate pins its own `nix` version, which is
    // not necessarily the one fcvm depends on.
    matches!(
        e,
        userfaultfd::Error::SystemError(errno) if (*errno as i32) == libc::EEXIST
    )
}

/// Whether a UFFDIO_CONTINUE error is the kernel saying "not now, try again".
///
/// The ioctl fails with `EAGAIN` (and zero progress) while the context's `mmap_changing`
/// flag is raised — an event-generating operation (fork, mremap, madvise) is mid-flight
/// and its event is sitting in the uffd queue waiting to be READ. The fault is NOT
/// resolved and the faulting vCPU stays asleep until a successful CONTINUE wakes it, so
/// the caller must drain the event queue (which is what lets the blocked operation finish
/// and clear `mmap_changing`) and retry. Dropping the fault = a permanently hung vCPU.
fn continue_would_block(e: &userfaultfd::Error) -> bool {
    matches!(
        e,
        userfaultfd::Error::SystemError(errno) if (*errno as i32) == libc::EAGAIN
    )
}

/// Whether a UFFDIO_CONTINUE error means the clone's mm is gone (process exited).
fn continue_vm_gone(e: &userfaultfd::Error) -> bool {
    matches!(
        e,
        userfaultfd::Error::SystemError(errno) if (*errno as i32) == libc::ESRCH
    )
}

/// Whether a UFFDIO_COPY error means the clone's mm is gone (process exited).
///
/// The COPY equivalent of [`continue_vm_gone`]. A clone that exits with a fault in flight
/// races the handler: the `read_event`-returns-error path (uffd closed) is the common
/// ending, but if the copy is already in the kernel it comes back `ESRCH` instead. That is
/// an ordinary end of life, NOT a service failure — and the distinction now matters, because
/// a failure here kills a VMM and reports the clone FAILED. Misreading a normal exit as a
/// failure would fill the serve log with false alarms and devalue the real ones.
fn copy_vm_gone(e: &userfaultfd::Error) -> bool {
    matches!(
        e,
        userfaultfd::Error::CopyFailed(errno) if (*errno as i32) == libc::ESRCH
    )
}

/// How one attempt at resolving a MINOR fault ended.
enum ContinueOutcome {
    /// The whole granule is mapped in the clone (by us, or by a racing fault — EEXIST).
    Resolved,
    /// `EAGAIN` with no progress: `mmap_changing` is set. The fault is still pending and
    /// MUST be retried after the event queue has been drained.
    Retry,
    /// `ESRCH`: the clone exited while we were resolving. Nothing left to serve.
    VmGone,
}

/// Resolve one faulting granule (`page_size` bytes: 4 KiB shmem / 2 MiB hugetlb) with
/// `UFFDIO_CONTINUE`, handling every outcome the kernel documents:
///
/// * `Ok(mapped)` — the kernel maps from the start of the range and reports the bytes it
///   installed. A short count means it stopped early (the ioctl signals this as `EAGAIN`
///   after partial progress); advance past the mapped bytes and continue the remainder
///   instead of discarding the count.
/// * `EEXIST` — the granule at the current position is already mapped (a racing guest
///   thread won). That sub-range is DONE; skip it, it is not an error.
/// * `EAGAIN` with zero progress — `mmap_changing` is set; report [`ContinueOutcome::Retry`]
///   so the caller drains the event queue and retries. Never drop the fault.
/// * `ESRCH` — the clone died; report [`ContinueOutcome::VmGone`].
/// * anything else — a real error; propagate loudly (a dropped fault is a hung vCPU).
fn continue_page(
    uffd: &Uffd,
    vm_id: &str,
    page: usize,
    page_size: usize,
) -> Result<ContinueOutcome> {
    let mut done = 0usize;
    while done < page_size {
        let addr = (page + done) as *mut std::ffi::c_void;
        let remaining = page_size - done;
        match uffd.r#continue(addr, remaining, true) {
            Ok(mapped) => {
                let mapped = usize::try_from(mapped).unwrap_or(0);
                // The kernel never reports zero-byte success; guard against a spin.
                anyhow::ensure!(
                    mapped > 0,
                    "UFFDIO_CONTINUE reported zero mapped bytes at 0x{:x}",
                    page + done
                );
                done += mapped;
            }
            Err(e) if continue_hit_present_page(&e) => {
                debug!(
                    target: "uffd",
                    vm_id = %vm_id,
                    fault_addr = format!("0x{:x}", page + done),
                    "UFFDIO_CONTINUE skipped - page already mapped (EEXIST)"
                );
                // EEXIST refers to the granule at the current position; with per-granule
                // requests that is the whole remaining range.
                done += remaining;
            }
            Err(e) if continue_would_block(&e) => return Ok(ContinueOutcome::Retry),
            Err(e) if continue_vm_gone(&e) => return Ok(ContinueOutcome::VmGone),
            Err(e) => {
                error!(
                    target: "uffd",
                    vm_id = %vm_id,
                    fault_addr = format!("0x{:x}", page + done),
                    error = ?e,
                    "UFFDIO_CONTINUE failed"
                );
                return Err(e.into());
            }
        }
    }
    Ok(ContinueOutcome::Resolved)
}

/// Handle page faults for a single VM
async fn handle_vm_page_faults(
    vm_id: String,
    uffd: Uffd,
    mappings: Vec<GuestRegionUffdMapping>,
    source: Arc<PageSource>,
) -> Result<()> {
    // Derive page size from mappings (all regions use the same page size)
    let page_size = mappings.first().map(|m| m.page_size).unwrap_or(4096);
    let page_mask = !(page_size - 1);

    info!(
        target: "uffd",
        vm_id = %vm_id,
        page_size,
        "page fault handler started"
    );

    // Set UFFD to non-blocking mode for async integration
    let uffd_fd = uffd.as_raw_fd();
    unsafe {
        let flags = libc::fcntl(uffd_fd, libc::F_GETFL);
        libc::fcntl(uffd_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }

    // Wrap UFFD in AsyncFd for tokio integration
    let async_uffd = AsyncFd::new(uffd).context("creating AsyncFd for UFFD")?;

    let mut fault_count = 0u64;
    let start_time = std::time::Instant::now();

    // MINOR faults whose UFFDIO_CONTINUE came back `EAGAIN` (`mmap_changing` was set),
    // keyed by page address, value = retry attempts so far. The faulting vCPU sleeps
    // until a successful CONTINUE wakes it and will never re-fault on its own, so every
    // entry here MUST eventually resolve or the handler must die loudly — silently
    // dropping one is a permanently hung guest.
    let mut pending_continues: std::collections::BTreeMap<usize, u32> =
        std::collections::BTreeMap::new();
    // How long to wait for new events before re-attempting parked CONTINUEs. Retries are
    // also attempted after every drain, so this only bounds the idle-queue case.
    const CONTINUE_RETRY_DELAY: Duration = Duration::from_millis(2);
    // Hard bound on retries per fault (~2s at CONTINUE_RETRY_DELAY): mmap_changing
    // clears as soon as the queued event is read, so persistent EAGAIN means something
    // is wedged. Failing the handler is loud; an infinite loop or a drop would not be.
    const MAX_CONTINUE_ATTEMPTS: u32 = 1000;

    loop {
        // Wait for UFFD to be readable — but never park indefinitely while resolved-later
        // faults are waiting, because no new event will ever arrive for THEM.
        let maybe_guard = if pending_continues.is_empty() {
            match async_uffd.readable().await {
                Ok(guard) => Some(guard),
                Err(e) => {
                    error!(target: "uffd", vm_id = %vm_id, error = %e, "error waiting for UFFD readability");
                    return Err(e.into());
                }
            }
        } else {
            match tokio::time::timeout(CONTINUE_RETRY_DELAY, async_uffd.readable()).await {
                Ok(Ok(guard)) => Some(guard),
                Ok(Err(e)) => {
                    error!(target: "uffd", vm_id = %vm_id, error = %e, "error waiting for UFFD readability");
                    return Err(e.into());
                }
                // Timeout: no new events. Fall through and retry the parked faults.
                Err(_) => None,
            }
        };

        // Set when the drain stopped on its batch limit rather than on an empty queue, so
        // this handler gives the runtime a turn before coming back for the rest.
        let mut yield_after_batch = false;
        if let Some(mut guard) = maybe_guard {
            match drain_events(
                &mut guard,
                &vm_id,
                &mappings,
                &source,
                page_size,
                page_mask,
                &mut fault_count,
                &mut pending_continues,
                start_time,
            )? {
                DrainOutcome::VmExited => return Ok(()),
                DrainOutcome::QueueDrained => guard.clear_ready(),
                // Events are still queued. Deliberately do NOT clear readiness: AsyncFd is
                // edge-triggered, and the kernel raises no new edge for events already
                // sitting in the queue, so clearing here could park this handler until the
                // guest happens to fault again — with faulting vCPUs already asleep.
                DrainOutcome::BatchFull => yield_after_batch = true,
            }
        }

        // Retry parked faults now that the queue is drained — reading the queued event is
        // exactly what allows the in-flight operation to finish and `mmap_changing` to
        // clear, so this is the earliest moment a retry can succeed.
        if !pending_continues.is_empty() {
            let mut resolved = Vec::new();
            for (&page, attempts) in pending_continues.iter_mut() {
                match continue_page(async_uffd.get_ref(), &vm_id, page, page_size)? {
                    ContinueOutcome::Resolved => resolved.push(page),
                    ContinueOutcome::VmGone => {
                        info!(target: "uffd", vm_id = %vm_id, "VM exited during CONTINUE retry");
                        return Ok(());
                    }
                    ContinueOutcome::Retry => {
                        *attempts += 1;
                        if *attempts >= MAX_CONTINUE_ATTEMPTS {
                            return Err(anyhow!(
                                "UFFDIO_CONTINUE at 0x{:x} still EAGAIN after {} attempts — \
                                 mmap_changing never cleared. Refusing to drop the fault \
                                 (a dropped fault is a permanently hung vCPU); failing the \
                                 handler for vm {} instead",
                                page,
                                MAX_CONTINUE_ATTEMPTS,
                                vm_id
                            ));
                        }
                    }
                }
            }
            for page in resolved {
                pending_continues.remove(&page);
            }
        }

        // Fairness: one clone's fault storm must not own a runtime worker forever. The
        // events left in the queue are picked up on the next pass (readiness was left set).
        if yield_after_batch {
            tokio::task::yield_now().await;
        }
    }
}

/// How a [`drain_events`] pass ended.
enum DrainOutcome {
    /// The uffd is closed: the clone's VMM exited and there is nothing left to serve.
    VmExited,
    /// The event queue is empty; readiness may be cleared and the handler may park.
    QueueDrained,
    /// [`MAX_EVENTS_PER_BATCH`] events were handled and more remain queued. The caller must
    /// leave readiness SET (edge-triggered epoll raises no new edge for queued events) and
    /// yield before draining the rest.
    BatchFull,
}

/// Drain up to [`MAX_EVENTS_PER_BATCH`] ready events from the uffd.
#[allow(clippy::too_many_arguments)]
fn drain_events(
    guard: &mut tokio::io::unix::AsyncFdReadyGuard<'_, Uffd>,
    vm_id: &str,
    mappings: &[GuestRegionUffdMapping],
    source: &PageSource,
    page_size: usize,
    page_mask: usize,
    fault_count: &mut u64,
    pending_continues: &mut std::collections::BTreeMap<usize, u32>,
    start_time: std::time::Instant,
) -> Result<DrainOutcome> {
    {
        let mut handled = 0usize;
        // Read available events (non-blocking), bounded by the batch size
        loop {
            if handled >= MAX_EVENTS_PER_BATCH {
                return Ok(DrainOutcome::BatchFull);
            }
            let event = match guard.get_inner().read_event() {
                Ok(Some(event)) => event,
                Ok(None) => break, // No more events ready
                Err(_) => {
                    // UFFD closed = VM exited
                    let elapsed = start_time.elapsed();
                    let rate = if elapsed.as_secs_f64() > 0.0 {
                        *fault_count as f64 / elapsed.as_secs_f64()
                    } else {
                        0.0
                    };
                    info!(
                        target: "uffd",
                        vm_id = %vm_id,
                        fault_count = *fault_count,
                        elapsed_secs = format!("{:.1}", elapsed.as_secs_f64()),
                        pages_per_sec = format!("{:.0}", rate),
                        "VM exited"
                    );
                    return Ok(DrainOutcome::VmExited);
                }
            };
            handled += 1;

            match event {
                Event::Pagefault { addr, kind, .. } => {
                    *fault_count += 1;

                    // Find which memory region this address belongs to
                    let fault_page = (addr as usize) & page_mask;

                    let mapping = mappings
                        .iter()
                        .find(|m| m.contains(fault_page as u64))
                        .ok_or_else(|| {
                            anyhow::anyhow!("page fault at unmapped address: 0x{:x}", fault_page)
                        })?;

                    let base_host = mapping.base_host_virt_addr as usize;
                    if fault_page < base_host {
                        return Err(anyhow!(
                            "page fault address 0x{:x} precedes mapping base 0x{:x}",
                            fault_page,
                            base_host
                        ));
                    }

                    let mmap = match source {
                        PageSource::Minor { .. } => {
                            // MINOR mode: the page is already in the page cache of the shared
                            // memfd that this clone mapped MAP_PRIVATE. UFFDIO_CONTINUE just
                            // installs a (read-only) PTE onto that folio — no copy, no file
                            // offset arithmetic, because the clone mapped the backing file at
                            // exactly the offsets the snapshot uses.
                            // Only UFFDIO_REGISTER_MODE_MINOR is registered, so the kernel can
                            // only ever deliver minor faults here. Holes in the backing memfd
                            // (all-zero snapshot pages, deliberately not written) are resolved
                            // by the kernel itself without any event, and the folio it
                            // allocates lands in the memfd's page cache so later clones DO
                            // fault MINOR onto it. Anything else means the registration mode
                            // and this handler have drifted apart.
                            if kind != FaultKind::Minor {
                                return Err(anyhow!(
                                    "unexpected {:?} fault at 0x{:x} in MINOR mode — Firecracker \
                                     registered a mode this handler cannot resolve",
                                    kind,
                                    fault_page
                                ));
                            }
                            match continue_page(guard.get_inner(), vm_id, fault_page, page_size)? {
                                ContinueOutcome::Resolved => {}
                                ContinueOutcome::VmGone => return Ok(DrainOutcome::VmExited),
                                ContinueOutcome::Retry => {
                                    // mmap_changing is set and the event that raised it is
                                    // somewhere in THIS queue. Don't spin here — park the
                                    // fault; the caller retries it right after the drain.
                                    debug!(
                                        target: "uffd",
                                        vm_id = %vm_id,
                                        fault_addr = format!("0x{:x}", fault_page),
                                        "UFFDIO_CONTINUE EAGAIN (mmap_changing) - parked for retry"
                                    );
                                    pending_continues.entry(fault_page).or_insert(0);
                                }
                            }
                            continue;
                        }
                        PageSource::Copy { mmap } => mmap,
                    };

                    let offset_in_region = fault_page - base_host;
                    let mapping_offset = usize::try_from(mapping.offset)
                        .map_err(|_| anyhow!("mapping offset exceeds host address space"))?;
                    let offset_in_file = mapping_offset
                        .checked_add(offset_in_region)
                        .ok_or_else(|| anyhow!("mapping offset overflow"))?;
                    let mmap_len = mmap.len();

                    if offset_in_file >= mmap_len {
                        warn!(
                            target: "uffd",
                            vm_id = %vm_id,
                            fault_addr = format!("0x{:x}", fault_page),
                            "page fault past end of snapshot memory, zero-filling page"
                        );
                        // Heap-allocate zero buffer (2MB on stack would overflow for hugepages)
                        let zero_page: Vec<u8> = vec![0u8; page_size];
                        let result = unsafe {
                            guard.get_inner().copy(
                                zero_page.as_ptr() as *const std::ffi::c_void,
                                fault_page as *mut std::ffi::c_void,
                                page_size,
                                true,
                            )
                        };
                        if let Err(e) = result {
                            if copy_vm_gone(&e) {
                                info!(target: "uffd", vm_id = %vm_id, "VM exited during zero-fill");
                                return Ok(DrainOutcome::VmExited);
                            }
                            error!(
                                target: "uffd",
                                vm_id = %vm_id,
                                fault_addr = format!("0x{:x}", fault_page),
                                error = ?e,
                                "UFFD zero-page copy failed"
                            );
                            return Err(e.into());
                        }
                        continue;
                    }

                    let bytes_available = mmap_len - offset_in_file;

                    let copy_result = if bytes_available >= page_size {
                        let page_data = &mmap[offset_in_file..offset_in_file + page_size];
                        unsafe {
                            guard.get_inner().copy(
                                page_data.as_ptr() as *const std::ffi::c_void,
                                fault_page as *mut std::ffi::c_void,
                                page_size,
                                true,
                            )
                        }
                    } else {
                        // Partial page at end of file: copy available data, zero-fill rest
                        // Heap-allocate (2MB on stack would overflow for hugepages)
                        let mut temp: Vec<u8> = vec![0u8; page_size];
                        temp[..bytes_available].copy_from_slice(
                            &mmap[offset_in_file..offset_in_file + bytes_available],
                        );
                        unsafe {
                            guard.get_inner().copy(
                                temp.as_ptr() as *const std::ffi::c_void,
                                fault_page as *mut std::ffi::c_void,
                                page_size,
                                true,
                            )
                        }
                    };

                    if let Err(e) = copy_result {
                        // EEXIST means page was already filled (race with another fault for same page)
                        // This is normal on older kernels with less aggressive page fault coalescing.
                        // See: https://docs.kernel.org/admin-guide/mm/userfaultfd.html
                        // "the kernel must cope with it returning -EEXIST from ioctl(UFFDIO_COPY) as expected"
                        if let userfaultfd::Error::CopyFailed(errno) = &e {
                            // Compare raw errno value since we may have different nix versions
                            if (*errno as i32) == libc::EEXIST {
                                debug!(
                                    target: "uffd",
                                    vm_id = %vm_id,
                                    fault_addr = format!("0x{:x}", fault_page),
                                    "UFFD copy skipped - page already filled (EEXIST)"
                                );
                                continue;
                            }
                        }

                        // The clone exited with this fault in flight — an ordinary end of
                        // life, not a service failure (which would now kill a VMM and
                        // report the clone FAILED). Same treatment as the MINOR path's
                        // `ContinueOutcome::VmGone`.
                        if copy_vm_gone(&e) {
                            info!(
                                target: "uffd",
                                vm_id = %vm_id,
                                fault_addr = format!("0x{:x}", fault_page),
                                "VM exited while its fault was being served"
                            );
                            return Ok(DrainOutcome::VmExited);
                        }

                        // Real error - log with Debug format to show errno
                        error!(
                            target: "uffd",
                            vm_id = %vm_id,
                            fault_addr = format!("0x{:x}", fault_page),
                            offset_in_file,
                            error = ?e,
                            "UFFD copy failed"
                        );
                        return Err(e.into());
                    }
                }
                Event::Remove { start, end } => {
                    // Balloon device removed pages - zero them
                    // Validate bounds: end must be >= start and range must be reasonable
                    let start_addr = start as usize;
                    let end_addr = end as usize;
                    if end_addr < start_addr {
                        warn!(
                            target: "uffd",
                            vm_id = %vm_id,
                            start = format!("0x{:x}", start_addr),
                            end = format!("0x{:x}", end_addr),
                            "Remove event with invalid range (end < start), ignoring"
                        );
                        continue;
                    }
                    let len = end_addr.saturating_sub(start_addr);
                    if len == 0 {
                        continue; // Nothing to zero
                    }

                    if matches!(source, PageSource::Minor { .. }) {
                        // This branch is expected to be DEAD in MINOR mode. Remove events are
                        // generated only by madvise(MADV_DONTNEED/MADV_REMOVE) on a registered
                        // range (fs/userfaultfd.c userfaultfd_remove), but a MINOR clone's
                        // guest memory is a file-backed MAP_PRIVATE mapping, and for those
                        // Firecracker's balloon path (vstate/memory.rs discard_range) does NOT
                        // madvise — it mmap()s fresh anonymous MAP_PRIVATE|MAP_FIXED memory
                        // over the range. That replaces the VMA: the range reads back as
                        // zeros (correct balloon semantics), the MINOR registration for it is
                        // torn down with the old VMA, and no uffd event is generated at all,
                        // so this server is simply never involved in that range again.
                        //
                        // If a Remove event nevertheless arrives (a future Firecracker
                        // switching that path back to madvise), no action is needed or
                        // possible: UFFDIO_ZEROPAGE is EINVAL on hugetlb VMAs and would
                        // defeat page sharing on shmem, and after MADV_DONTNEED on a private
                        // file mapping the next touch minor-faults again and gets served the
                        // pristine snapshot page — an acceptable post-discard state, since
                        // balloon-discarded contents are undefined for the guest. Warn so
                        // drift from the expected Firecracker behaviour is visible.
                        warn!(
                            target: "uffd",
                            vm_id = %vm_id,
                            start = format!("0x{:x}", start_addr),
                            len,
                            "unexpected Remove event in MINOR mode (Firecracker balloon \
                             discard should replace the VMA, not madvise) - ignoring; \
                             still-registered pages will minor-fault normally"
                        );
                        continue;
                    }

                    // Remove events and page faults for the same range arrive in either order,
                    // so a page in this range may already have been filled by UFFDIO_COPY before
                    // we see the Remove event. UFFDIO_ZEROPAGE then fails with EEXIST (first page
                    // already present) or EAGAIN (range partially zeroed before hitting a present
                    // page). Tolerate both — same as the EEXIST handling in the copy path above —
                    // by falling back to per-page zeroing that skips already-present pages.
                    // Killing the handler here would close the uffd and silently corrupt the
                    // still-running VM.
                    let bulk_result = unsafe { guard.get_inner().zeropage(start, len, true) };
                    if let Err(e) = bulk_result {
                        if !zeropage_hit_present_page(&e) {
                            error!(
                                target: "uffd",
                                vm_id = %vm_id,
                                start = format!("0x{:x}", start_addr),
                                len,
                                error = ?e,
                                "UFFD zeropage failed for Remove event"
                            );
                            return Err(e.into());
                        }
                        debug!(
                            target: "uffd",
                            vm_id = %vm_id,
                            start = format!("0x{:x}", start_addr),
                            len,
                            error = ?e,
                            "bulk zeropage hit already-present pages, zeroing per page"
                        );
                        let mut page = start_addr;
                        while page < end_addr {
                            let page_result = unsafe {
                                guard.get_inner().zeropage(
                                    page as *mut std::ffi::c_void,
                                    page_size,
                                    true,
                                )
                            };
                            if let Err(page_err) = page_result {
                                if !zeropage_hit_present_page(&page_err) {
                                    error!(
                                        target: "uffd",
                                        vm_id = %vm_id,
                                        page = format!("0x{:x}", page),
                                        error = ?page_err,
                                        "UFFD zeropage failed for Remove event"
                                    );
                                    return Err(page_err.into());
                                }
                                debug!(
                                    target: "uffd",
                                    vm_id = %vm_id,
                                    page = format!("0x{:x}", page),
                                    "zeropage skipped - page already present"
                                );
                            }
                            page += page_size;
                        }
                    }
                }
                Event::Fork { .. } | Event::Remap { .. } | Event::Unmap { .. } => {
                    // Ignore these events
                }
            }
        }
    }
    Ok(DrainOutcome::QueueDrained)
}

/// Memory region mapping from Firecracker.
///
/// Firecracker sends these in the UFFD handshake JSON. The `page_size` field
/// indicates the page granularity for this region:
/// - 4096 (4KB): standard pages
/// - 2097152 (2MB): hugepage-backed memory (`huge_pages: "2M"`)
/// - 16384 (16KB): ARM64 with CONFIG_ARM64_16K_PAGES (future)
///
/// The `page_size` field is required — our Firecracker fork always sends it.
#[derive(Debug, serde::Deserialize)]
struct GuestRegionUffdMapping {
    base_host_virt_addr: u64,
    size: usize,
    offset: u64,
    /// Page size for this region (from Firecracker handshake).
    /// Standard: 4096, hugepages: 2097152.
    page_size: usize,
}

impl GuestRegionUffdMapping {
    /// Check if address is within this mapping (overflow-safe)
    fn contains(&self, addr: u64) -> bool {
        if addr < self.base_host_virt_addr {
            return false;
        }
        // Use checked arithmetic to prevent overflow
        match self.base_host_virt_addr.checked_add(self.size as u64) {
            Some(end) => addr < end,
            None => true, // If overflow, assume addr is within (max range)
        }
    }

    /// Validate that this mapping has sensible values
    fn validate(&self) -> Result<()> {
        if self.size == 0 {
            anyhow::bail!(
                "mapping has zero size at base 0x{:x}",
                self.base_host_virt_addr
            );
        }
        // Validate page_size is a non-zero power of two
        if self.page_size == 0 || !self.page_size.is_power_of_two() {
            anyhow::bail!(
                "invalid page_size {}: must be a non-zero power of two",
                self.page_size
            );
        }
        // Check for overflow in base + size
        if self
            .base_host_virt_addr
            .checked_add(self.size as u64)
            .is_none()
        {
            anyhow::bail!(
                "mapping range overflow: base 0x{:x}, size {}",
                self.base_host_virt_addr,
                self.size
            );
        }
        Ok(())
    }
}

/// Complete the handshake with a newly connected Firecracker.
///
/// MINOR mode is a two-message exchange (fcvm speaks first):
/// 1. fcvm sends the shared backing memfd over `SCM_RIGHTS`,
/// 2. Firecracker maps it `MAP_PRIVATE`, registers a userfaultfd in
///    `UFFDIO_REGISTER_MODE_MINOR`, and replies with the mappings JSON + that uffd.
///
/// COPY mode skips step 1 — Firecracker allocates anonymous memory itself and sends only
/// the mappings + uffd.
async fn handshake(
    stream: tokio::net::UnixStream,
    source: &PageSource,
) -> Result<(Uffd, Vec<GuestRegionUffdMapping>)> {
    let std_stream = stream.into_std().context("converting to std stream")?;
    // Keep non-blocking — AsyncFd handles readiness
    let async_stream = AsyncFd::new(std_stream).context("creating AsyncFd for handshake socket")?;

    if let PageSource::Minor { backing } = source {
        send_backing_fd(&async_stream, backing).await?;
    }

    // 4096 bytes for JSON message buffer (unrelated to page size)
    let mut message_buf = vec![0u8; 4096];

    // Wait for data to arrive, then recv with fd passing
    let (bytes_read, uffd_fd_opt) = loop {
        let mut guard = async_stream.readable().await?;
        match guard.get_inner().recv_with_fd(&mut message_buf) {
            Ok(result) => break result,
            Err(e) if e.errno() == libc::EWOULDBLOCK || e.errno() == libc::EAGAIN => {
                guard.clear_ready();
                continue;
            }
            Err(e) => return Err(e).context("receiving UFFD from Firecracker"),
        }
    };

    let uffd_file = uffd_fd_opt.ok_or_else(|| anyhow!("no UFFD file descriptor received"))?;

    message_buf.resize(bytes_read, 0);

    // Parse JSON message containing memory region mappings
    let message = String::from_utf8(message_buf).context("parsing message as UTF-8")?;
    let mappings: Vec<GuestRegionUffdMapping> =
        serde_json::from_str(&message).context("parsing memory mappings JSON")?;

    // Validate all received mappings
    if mappings.is_empty() {
        anyhow::bail!("received empty memory mappings from Firecracker");
    }
    for (i, mapping) in mappings.iter().enumerate() {
        mapping
            .validate()
            .with_context(|| format!("invalid mapping at index {}", i))?;
    }

    // Convert File to Uffd
    let uffd = unsafe { Uffd::from_raw_fd(uffd_file.into_raw_fd()) };

    Ok((uffd, mappings))
}

/// Send the shared backing memfd to a connecting Firecracker over `SCM_RIGHTS`.
async fn send_backing_fd(
    async_stream: &AsyncFd<std::os::unix::net::UnixStream>,
    backing: &File,
) -> Result<()> {
    // Payload is ignored by the receiver; SCM_RIGHTS needs at least one data byte.
    const HELLO: &[u8] = b"FCVM_UFFD_MINOR_BACKING";
    loop {
        let mut guard = async_stream.writable().await?;
        match guard.get_inner().send_with_fd(HELLO, backing.as_raw_fd()) {
            Ok(_) => return Ok(()),
            Err(e) if e.errno() == libc::EWOULDBLOCK || e.errno() == libc::EAGAIN => {
                guard.clear_ready();
                continue;
            }
            Err(e) => return Err(e).context("sending backing memfd to Firecracker"),
        }
    }
}

/// Create, populate, **seal**, and reopen read-only the shared memfd that MINOR-mode
/// clones map `MAP_PRIVATE`.
///
/// The returned fd is what every clone receives over `SCM_RIGHTS`, so it must not be able
/// to modify the golden snapshot: kernel CoW only covers *mapped* writes, and an unsealed
/// fd could `pwrite(2)`/`ftruncate(2)` the backing file — or be reopened `O_RDWR` via
/// `/proc/self/fd` — corrupting the snapshot for every other clone. Two independent locks
/// close that door:
///
/// * the inode is sealed `F_SEAL_SEAL|F_SEAL_SHRINK|F_SEAL_GROW|F_SEAL_WRITE` once
///   populated (verified to work for `MFD_HUGETLB` on this kernel), which makes every
///   write path fail no matter how the fd is reopened: `write(2)` EPERM (hugetlbfs has no
///   write support at all and fails EINVAL even unsealed), `ftruncate(2)` EPERM,
///   `mmap(MAP_SHARED, PROT_WRITE)` EPERM;
/// * the fd actually handed out is an `O_RDONLY` reopen, so plain `pwrite(2)` on it is
///   EBADF before the seals even get a say.
///
/// `MAP_PRIVATE` mappings (any PROT) and `UFFDIO_CONTINUE` are unaffected by both —
/// proven by `test_backing_memfd_is_sealed_and_serves_minor_faults` and the
/// `/tmp/uffdproto/seal_reserve.c` kernel probe.
///
/// The memfd is resident for as long as the server lives (shmem is unswappable on a host
/// with no swap), so it is the fixed cost of the whole scheme — every byte written here is
/// paid once and then shared by every clone. Two rules govern what gets written:
///
/// * **All-zero pages are left as holes (shmem only).** The first clone to read a hole takes
///   an ordinary fault; `shmem_fault` allocates the folio *in the inode's page cache*
///   (`shmem_get_folio_gfp(..., SGP_CACHE, ...)`) regardless of `VM_SHARED`, so every later
///   clone finds it there and faults MINOR onto the same physical page. Sharing is preserved
///   and the resident cost drops to the snapshot's non-zero footprint (measured: 63% of a
///   1 GiB idle alpine/nginx snapshot is zero pages).
/// * **hugetlb is populated in full.** `hugetlb_no_page()` only calls
///   `hugetlb_add_to_page_cache()` when `vma->vm_flags & VM_MAYSHARE`; on our `MAP_PRIVATE`
///   VMA a hole fault allocates an *anonymous* huge page instead, one per clone. Leaving
///   holes there would silently destroy the sharing this whole path exists for.
fn create_backing_memfd(
    snapshot_id: &str,
    mut mem_file: File,
    mem_size: usize,
    hugepages: bool,
) -> Result<File> {
    use std::io::Read;

    let page_size = if hugepages { HUGE_PAGE_2M } else { 4096 };
    // hugetlbfs rounds allocations to whole huge pages; size the memfd accordingly so the
    // final partial page is still fully backed.
    let backing_size = mem_size.div_ceil(page_size) * page_size;

    if hugepages {
        preflight_hugepages(
            backing_size / HUGE_PAGE_2M,
            "a MINOR-mode snapshot backing (hugetlb backings are populated in full)",
        )?;
    }

    let name = std::ffi::CString::new(format!("fcvm-snap-{snapshot_id}"))
        .context("building memfd name")?;
    let mut flags = libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING;
    if hugepages {
        flags |= libc::MFD_HUGETLB | libc::MFD_HUGE_2MB;
    }
    // SAFETY: `name` is a valid NUL-terminated string that outlives the call.
    let fd = unsafe { libc::memfd_create(name.as_ptr(), flags) };
    if fd < 0 {
        let err = std::io::Error::last_os_error();
        anyhow::bail!(
            "memfd_create(hugetlb={hugepages}) failed: {err} — MINOR-mode restore needs a \
             shmem or hugetlbfs backing file"
        );
    }
    // SAFETY: `fd` is a fresh, owned file descriptor.
    let backing = unsafe { File::from_raw_fd(fd) };
    backing
        .set_len(backing_size as u64)
        .context("sizing backing memfd")?;

    // Populate: mmap the memfd MAP_SHARED and stream the snapshot file into it.
    let mut dest = unsafe {
        memmap2::MmapOptions::new()
            .len(backing_size)
            .map_mut(&backing)
            .context("mmapping backing memfd for population")?
    };

    let start = std::time::Instant::now();
    // Read through a scratch buffer so an all-zero page can be SKIPPED: touching `dest`
    // is what allocates a folio, so not touching it leaves a hole.
    let chunk_len = (8 * 1024 * 1024usize).next_multiple_of(page_size);
    let mut scratch = vec![0u8; chunk_len];
    let zero_page = vec![0u8; page_size];
    let mut offset = 0usize;
    let mut resident_pages = 0usize;
    let mut hole_pages = 0usize;
    while offset < mem_size {
        let end = (offset + chunk_len).min(mem_size);
        let len = end - offset;
        mem_file
            .read_exact(&mut scratch[..len])
            .with_context(|| format!("reading snapshot memory at offset {offset}"))?;
        let mut p = 0usize;
        while p < len {
            let plen = page_size.min(len - p);
            let src = &scratch[p..p + plen];
            if !hugepages && plen == page_size && src == &zero_page[..] {
                hole_pages += 1;
            } else {
                dest[offset + p..offset + p + plen].copy_from_slice(src);
                resident_pages += 1;
            }
            p += plen;
        }
        offset = end;
    }
    // Tail beyond the snapshot file (hugetlb rounding) stays zero — `set_len` guarantees it.
    // Must be unmapped BEFORE sealing: F_SEAL_WRITE refuses while writable shared
    // mappings exist.
    drop(dest);

    // Seal the inode so nothing — not this fd, not any /proc/self/fd reopen of it in a
    // clone — can ever modify the populated snapshot again.
    let seals = libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;
    // SAFETY: fcntl(F_ADD_SEALS) on an owned memfd created with MFD_ALLOW_SEALING; the
    // only writable mapping was just dropped.
    if unsafe { libc::fcntl(backing.as_raw_fd(), libc::F_ADD_SEALS, seals) } < 0 {
        return Err(std::io::Error::last_os_error()).context("sealing backing memfd (F_ADD_SEALS)");
    }
    // SAFETY: fcntl(F_GET_SEALS) on an owned fd.
    let got = unsafe { libc::fcntl(backing.as_raw_fd(), libc::F_GET_SEALS) };
    anyhow::ensure!(
        got >= 0 && (got & seals) == seals,
        "backing memfd seals did not stick (want {seals:#x}, got {got:#x}) — refusing to hand \
         a writable snapshot fd to clones"
    );

    // Hand out an O_RDONLY reopen of the sealed inode, not the O_RDWR original.
    let backing_ro = File::open(format!("/proc/self/fd/{}", backing.as_raw_fd()))
        .context("reopening sealed backing memfd read-only")?;
    drop(backing);

    // The snapshot file's page cache is dead weight now: every clone reads the memfd.
    // SAFETY: fadvise on a valid fd with a whole-file range.
    unsafe {
        libc::posix_fadvise(mem_file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED);
    }

    info!(
        target: "uffd",
        snapshot = %snapshot_id,
        mem_size_mb = mem_size / (1024 * 1024),
        backing_mb = backing_size / (1024 * 1024),
        resident_mb = resident_pages * page_size / (1024 * 1024),
        hole_mb = hole_pages * page_size / (1024 * 1024),
        hugepages,
        populate_ms = start.elapsed().as_millis(),
        "populated, sealed and reopened read-only the shared backing memfd for MINOR-mode restore"
    );

    Ok(backing_ro)
}

/// Refuse an allocation the host hugepage pool cannot hold.
///
/// hugetlb pages are neither swappable nor reclaimable, so exhaustion is not "slow" — it
/// is `ENOMEM`/`SIGBUS`. Checking `Free - Rsvd` up front turns that into an explanatory
/// error at the point of the decision instead of a dead process later.
fn preflight_hugepages(needed_pages: usize, what: &str) -> Result<()> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").context("reading /proc/meminfo")?;
    let field = |name: &str| -> Option<usize> {
        meminfo.lines().find_map(|l| {
            let rest = l.strip_prefix(name)?;
            rest.split_whitespace().next()?.parse().ok()
        })
    };
    let free = field("HugePages_Free:").unwrap_or(0);
    let rsvd = field("HugePages_Rsvd:").unwrap_or(0);
    let usable = free.saturating_sub(rsvd);
    if usable < needed_pages {
        anyhow::bail!(
            "hugepage pool too small for {what}: need {needed_pages} x 2MiB ({} MiB) but only \
             {usable} are usable (HugePages_Free={free}, HugePages_Rsvd={rsvd}). \
             Raise vm.nr_hugepages.",
            needed_pages * 2
        );
    }
    Ok(())
}

/// Admission check for spawning one hugepage-backed UFFD clone.
///
/// Every hugepage clone can consume up to the FULL guest size in private huge pages: in
/// MINOR mode each guest write CoWs a 2 MiB page out of the shared backing (and the
/// kernel *reserves* the full range at restore — see `guest_memory_from_uffd_minor` in
/// the Firecracker fork — so an unservable clone fails its mmap with ENOMEM instead of
/// taking a SIGBUS mid-run); in COPY mode every faulted page is a fresh private huge
/// page. This check exists to fail with an explanation *before* spawning Firecracker.
/// It is advisory (another clone can win the race for the same pages); the authoritative,
/// race-free gate for MINOR clones is the kernel's hugetlb reservation at restore time.
pub fn preflight_clone_hugepages(memory_mib: usize) -> Result<()> {
    let needed_pages = (memory_mib * 1024 * 1024).div_ceil(HUGE_PAGE_2M);
    preflight_hugepages(
        needed_pages,
        "a hugepage-backed clone (worst case every guest page is privately copied-on-write; \
         MINOR-mode restore reserves the full guest size up front so a running guest can \
         never SIGBUS)",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mapping_contains_basic() {
        let mapping = GuestRegionUffdMapping {
            base_host_virt_addr: 0x1000,
            size: 0x1000, // 4KB
            offset: 0,
            page_size: 4096,
        };

        // Before mapping
        assert!(!mapping.contains(0x0FFF));
        // Start of mapping
        assert!(mapping.contains(0x1000));
        // Middle of mapping
        assert!(mapping.contains(0x1500));
        // Last byte of mapping
        assert!(mapping.contains(0x1FFF));
        // Just after mapping
        assert!(!mapping.contains(0x2000));
        // Way after mapping
        assert!(!mapping.contains(0x3000));
    }

    #[test]
    fn test_mapping_contains_large_address() {
        // Test with addresses near u64::MAX to verify overflow handling
        let mapping = GuestRegionUffdMapping {
            base_host_virt_addr: u64::MAX - 0x1000,
            size: 0x800,
            offset: 0,
            page_size: 4096,
        };

        // Should contain addresses within range
        assert!(mapping.contains(u64::MAX - 0x1000));
        assert!(mapping.contains(u64::MAX - 0x900));

        // Should not contain addresses before range
        assert!(!mapping.contains(u64::MAX - 0x1001));
    }

    #[test]
    fn test_mapping_contains_overflow() {
        // Test case where base + size would overflow u64
        let mapping = GuestRegionUffdMapping {
            base_host_virt_addr: u64::MAX - 100,
            size: 200, // This would overflow
            offset: 0,
            page_size: 4096,
        };

        // With overflow, contains() returns true for addresses >= base
        assert!(mapping.contains(u64::MAX - 100));
        assert!(mapping.contains(u64::MAX));
        // Still false for addresses before base
        assert!(!mapping.contains(u64::MAX - 101));
    }

    #[test]
    fn test_mapping_validate_success() {
        let mapping = GuestRegionUffdMapping {
            base_host_virt_addr: 0x1000,
            size: 0x1000,
            offset: 0,
            page_size: 4096,
        };
        assert!(mapping.validate().is_ok());
    }

    #[test]
    fn test_mapping_validate_zero_size() {
        let mapping = GuestRegionUffdMapping {
            base_host_virt_addr: 0x1000,
            size: 0, // Invalid
            offset: 0,
            page_size: 4096,
        };
        let result = mapping.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("zero size"));
    }

    #[test]
    fn test_mapping_validate_overflow() {
        let mapping = GuestRegionUffdMapping {
            base_host_virt_addr: u64::MAX - 100,
            size: 200, // Would overflow
            offset: 0,
            page_size: 4096,
        };
        let result = mapping.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("overflow"));
    }

    #[test]
    fn test_mapping_validate_invalid_page_size() {
        let mapping = GuestRegionUffdMapping {
            base_host_virt_addr: 0x1000,
            size: 0x1000,
            offset: 0,
            page_size: 0,
        };
        let result = mapping.validate();
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("invalid page_size"));

        let mapping = GuestRegionUffdMapping {
            base_host_virt_addr: 0x1000,
            size: 0x1000,
            offset: 0,
            page_size: 123, // Not a power of two
        };
        let result = mapping.validate();
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("invalid page_size"));
    }

    #[test]
    fn test_mapping_json_with_page_size() {
        // Firecracker sends page_size in UFFD handshake
        let json = r#"[
            {"base_host_virt_addr": 140000000, "size": 536870912, "offset": 0, "page_size": 2097152}
        ]"#;
        let mappings: Vec<GuestRegionUffdMapping> = serde_json::from_str(json).unwrap();
        assert_eq!(mappings.len(), 1);
        assert_eq!(mappings[0].base_host_virt_addr, 140000000);
        assert_eq!(mappings[0].size, 536870912); // 512MB
        assert_eq!(mappings[0].offset, 0);
        assert_eq!(mappings[0].page_size, 2097152); // 2MB hugepages
    }

    #[test]
    fn test_mapping_json_with_4k_page_size() {
        let json = r#"[
            {"base_host_virt_addr": 140000000, "size": 536870912, "offset": 0, "page_size": 4096}
        ]"#;
        let mappings: Vec<GuestRegionUffdMapping> = serde_json::from_str(json).unwrap();
        assert_eq!(mappings[0].page_size, 4096);
    }

    #[test]
    fn test_mapping_json_with_16k_page_size() {
        // Future-proofing: ARM64 CONFIG_ARM64_16K_PAGES
        let json = r#"[
            {"base_host_virt_addr": 140000000, "size": 536870912, "offset": 0, "page_size": 16384}
        ]"#;
        let mappings: Vec<GuestRegionUffdMapping> = serde_json::from_str(json).unwrap();
        assert_eq!(mappings[0].page_size, 16384);
    }

    #[test]
    fn test_mapping_json_multiple_regions() {
        let json = r#"[
            {"base_host_virt_addr": 140000000, "size": 268435456, "offset": 0, "page_size": 4096},
            {"base_host_virt_addr": 408435456, "size": 268435456, "offset": 268435456, "page_size": 4096}
        ]"#;
        let mappings: Vec<GuestRegionUffdMapping> = serde_json::from_str(json).unwrap();
        assert_eq!(mappings.len(), 2);

        // First region
        assert_eq!(mappings[0].size, 268435456); // 256MB
        assert_eq!(mappings[0].offset, 0);

        // Second region
        assert_eq!(mappings[1].offset, 268435456); // Starts after first
    }

    #[test]
    fn test_mapping_contains_with_hugepage_alignment() {
        // 2MB-aligned mapping
        let mapping = GuestRegionUffdMapping {
            base_host_virt_addr: 0x200000, // 2MB aligned
            size: 0x200000,                // 2MB
            offset: 0,
            page_size: 2097152,
        };
        assert!(mapping.contains(0x200000));
        assert!(mapping.contains(0x300000));
        assert!(!mapping.contains(0x400000));
    }

    // =========================================================================
    // Backing-memfd immutability (the fd every clone receives must not be able
    // to modify the golden snapshot) + MINOR/CONTINUE off the sealed fd.
    // =========================================================================

    /// Write a 3-page snapshot file: page0 = 0xAB, page1 = zeros (hole candidate),
    /// page2 = 0xCD. Returns (path, mem_size).
    fn write_test_snapshot() -> (std::path::PathBuf, usize) {
        use std::io::Write;
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "fcvm-seal-test-{}-{}.mem",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let mut f = File::create(&path).unwrap();
        f.write_all(&[0xABu8; 4096]).unwrap();
        f.write_all(&[0u8; 4096]).unwrap();
        f.write_all(&[0xCDu8; 4096]).unwrap();
        f.sync_all().unwrap();
        (path, 3 * 4096)
    }

    fn errno_of(res: isize) -> i32 {
        assert!(res < 0, "operation unexpectedly succeeded");
        std::io::Error::last_os_error().raw_os_error().unwrap()
    }

    /// Every write door on a backing fd must be shut. The expected errno differs by which
    /// lock stops the door first: for the O_RDONLY reopen we hand out, the fd mode does
    /// (pwrite EBADF, writable shared mmap EACCES); for a sneaky O_RDWR /proc reopen only
    /// the SEAL is left to do it (EPERM everywhere).
    fn assert_fd_cannot_write(
        fd: std::os::unix::io::RawFd,
        expect_pwrite_errno: i32,
        expect_mmap_errno: i32,
    ) {
        let byte = [0x5Au8];
        // pwrite(2)
        let r = unsafe { libc::pwrite(fd, byte.as_ptr().cast(), 1, 0) };
        assert_eq!(
            errno_of(r as isize),
            expect_pwrite_errno,
            "pwrite must be refused"
        );
        // ftruncate(2): shrink (destroys pages other clones are mapping)
        let r = unsafe { libc::ftruncate(fd, 4096) };
        let e = errno_of(r as isize);
        assert!(
            e == libc::EPERM || e == libc::EINVAL,
            "ftruncate(shrink) must be refused, got errno {e}"
        );
        // ftruncate(2): grow
        let r = unsafe { libc::ftruncate(fd, 1024 * 1024 * 1024) };
        let e = errno_of(r as isize);
        assert!(
            e == libc::EPERM || e == libc::EINVAL,
            "ftruncate(grow) must be refused, got errno {e}"
        );
        // mmap(MAP_SHARED, PROT_WRITE) — the door the MAP_SHARED experiment walked through
        let r = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                4096,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        assert_eq!(
            r,
            libc::MAP_FAILED,
            "writable shared mapping must be refused"
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error().unwrap(),
            expect_mmap_errno,
            "writable shared mapping must be refused by fd mode (EACCES) or seal (EPERM)"
        );
    }

    /// The fd `create_backing_memfd` returns (== what `send_backing_fd` hands every
    /// clone) cannot pwrite/ftruncate/map-shared-writable the snapshot, even via a
    /// /proc/self/fd O_RDWR reopen, while read paths still see the snapshot bytes.
    #[test]
    fn test_backing_memfd_is_sealed_and_read_only() {
        let (path, mem_size) = write_test_snapshot();
        let mem_file = File::open(&path).unwrap();
        let backing = create_backing_memfd("seal-test", mem_file, mem_size, false).unwrap();
        std::fs::remove_file(&path).unwrap();

        // All four seals present.
        let seals = unsafe { libc::fcntl(backing.as_raw_fd(), libc::F_GET_SEALS) };
        let want = libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;
        assert_eq!(seals & want, want, "expected all seals, got {seals:#x}");

        // The handed-out fd is O_RDONLY: pwrite dies on the fd mode (EBADF).
        assert_fd_cannot_write(backing.as_raw_fd(), libc::EBADF, libc::EACCES);

        // Reopening it O_RDWR through /proc (what a malicious/buggy clone could do)
        // must ALSO be unable to write — that is the seal doing its job.
        let rw = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(format!("/proc/self/fd/{}", backing.as_raw_fd()))
            .expect("O_RDWR reopen of a sealed memfd is allowed to open");
        assert_fd_cannot_write(rw.as_raw_fd(), libc::EPERM, libc::EPERM);

        // Read paths still work: MAP_PRIVATE sees the snapshot (and the zero page
        // stayed a hole but reads back as zeros).
        let map = unsafe { MmapOptions::new().map_copy(&backing).unwrap() };
        assert!(map[..4096].iter().all(|&b| b == 0xAB));
        assert!(map[4096..8192].iter().all(|&b| b == 0));
        assert!(map[8192..].iter().all(|&b| b == 0xCD));
    }

    /// Full-fidelity clone view: receive the backing fd over the real `send_backing_fd`
    /// SCM_RIGHTS path, then prove the RECEIVED fd cannot write — and that MAP_PRIVATE +
    /// UFFD MINOR registration + `continue_page` (UFFDIO_CONTINUE incl. the EEXIST race)
    /// all still work off the sealed, read-only fd.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_backing_memfd_is_sealed_and_serves_minor_faults() {
        let (path, mem_size) = write_test_snapshot();
        let mem_file = File::open(&path).unwrap();
        let backing = create_backing_memfd("seal-minor-test", mem_file, mem_size, false).unwrap();
        std::fs::remove_file(&path).unwrap();

        // --- receive the fd exactly the way a clone's Firecracker does ---
        let (tx, rx) = std::os::unix::net::UnixStream::pair().unwrap();
        tx.set_nonblocking(true).unwrap();
        let tx = AsyncFd::new(tx).unwrap();
        send_backing_fd(&tx, &backing).await.unwrap();
        let mut hello = [0u8; 64];
        let (_, received) = rx.recv_with_fd(&mut hello).unwrap();
        let received = received.expect("backing fd must arrive with the hello message");

        // The received fd must be unable to modify the snapshot in every way.
        assert_fd_cannot_write(received.as_raw_fd(), libc::EBADF, libc::EACCES);

        // --- MAP_PRIVATE + MINOR + CONTINUE off the received fd ---
        let mut map = unsafe { MmapOptions::new().map_copy(&received).unwrap() };
        let base = map.as_mut_ptr() as usize;
        let uffd = userfaultfd::UffdBuilder::new()
            .close_on_exec(true)
            .non_blocking(false)
            .user_mode_only(true)
            .create()
            .expect("creating userfaultfd (via /dev/userfaultfd)");
        uffd.register_with_mode(
            base as *mut std::ffi::c_void,
            mem_size,
            userfaultfd::RegisterMode::MINOR,
        )
        .expect("MINOR registration on a MAP_PRIVATE mapping of the sealed fd");

        // Touch page 2 from another thread -> MINOR fault -> resolve with continue_page.
        let reader = std::thread::spawn(move || unsafe {
            std::ptr::read_volatile((base + 8192) as *const u8)
        });
        let event = uffd
            .read_event()
            .unwrap()
            .expect("blocking read yields an event");
        let fault_addr = match event {
            Event::Pagefault {
                kind: FaultKind::Minor,
                addr,
                ..
            } => (addr as usize) & !0xFFF,
            other => panic!("expected a MINOR pagefault, got {other:?}"),
        };
        assert_eq!(fault_addr, base + 8192);
        match continue_page(&uffd, "seal-test", fault_addr, 4096).unwrap() {
            ContinueOutcome::Resolved => {}
            _ => panic!("continue_page must resolve a pending MINOR fault"),
        }
        assert_eq!(
            reader.join().unwrap(),
            0xCD,
            "fault must be served snapshot content"
        );

        // The EEXIST race (page already mapped) is success, not an error.
        match continue_page(&uffd, "seal-test", fault_addr, 4096).unwrap() {
            ContinueOutcome::Resolved => {}
            _ => panic!("EEXIST on an already-mapped page must count as resolved"),
        }

        // Guest-side CoW still works and never reaches the backing file.
        map[8192] = 0x11;
        assert_eq!(map[8192], 0x11);
        let pristine = unsafe { MmapOptions::new().map_copy_read_only(&received).unwrap() };
        assert_eq!(
            pristine[8192], 0xCD,
            "clone write must not reach the snapshot"
        );
    }

    // =========================================================================
    // FAIL CLOSED: a clone whose faults cannot be served must end up DEAD, not
    // running on zero-filled memory.
    // =========================================================================

    /// Spawn a harmless long-lived process to stand in for a clone's VMM.
    ///
    /// The production code path under test is identical either way — `PeerVmm` is just a
    /// pinned handle to "the process on the other end of this connection" — so the only
    /// substitution is WHICH process gets killed. Using a real child (rather than the test
    /// process) is what lets the test assert the kill actually landed.
    fn spawn_victim() -> std::process::Child {
        std::process::Command::new("sleep")
            .arg("600")
            .spawn()
            .expect("spawning the stand-in VMM process")
    }

    fn killed_by_sigkill(status: std::process::ExitStatus) -> bool {
        use std::os::unix::process::ExitStatusExt;
        status.signal() == Some(libc::SIGKILL)
    }

    /// Wait (bounded) for the stand-in VMM to die, then assert it died by SIGKILL.
    ///
    /// Bounded on purpose: the regression this guards against leaves the process ALIVE, and
    /// an unbounded wait would turn that into a hung test instead of a failing one. Verified
    /// by disabling the kill — the test then fails here in ~5s instead of blocking forever,
    /// and the stand-in process is reaped rather than leaked.
    async fn assert_vmm_was_killed(mut victim: std::process::Child) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            match victim.try_wait().expect("polling the stand-in VMM") {
                Some(status) => {
                    assert!(
                        killed_by_sigkill(status),
                        "an unservable clone's VMM must die by SIGKILL, got {status:?}"
                    );
                    return;
                }
                None if tokio::time::Instant::now() >= deadline => {
                    let _ = victim.kill();
                    let _ = victim.wait();
                    panic!(
                        "the clone's VMM is STILL RUNNING after its UFFD service failed — \
                         it would keep executing on zero-filled guest memory (the exact \
                         silent corruption this fail-closed path exists to prevent)"
                    );
                }
                None => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    }

    /// `PeerVmm::from_stream` must name the process that actually opened the connection —
    /// everything downstream (including a SIGKILL) is aimed at that PID.
    #[tokio::test]
    async fn test_peer_vmm_identifies_the_connecting_process() {
        let dir = std::env::temp_dir().join(format!("fcvm-peer-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("peer.sock");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();

        let connector = tokio::spawn({
            let path = path.clone();
            async move { UnixStream::connect(&path).await.unwrap() }
        });
        let (server_side, _) = listener.accept().await.unwrap();
        let _client_side = connector.await.unwrap();

        let peer = PeerVmm::from_stream(&server_side).expect("peer must be pinnable");
        assert_eq!(
            peer.pid,
            std::process::id(),
            "the pinned pidfd must refer to the connecting process"
        );
        assert!(peer.is_alive(), "liveness must be answered by the pidfd");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The peer handle must come from the SOCKET in one step. `SO_PEERCRED` + `pidfd_open`
    /// would leave a window in which the peer exits, its PID is recycled, and the pidfd ends
    /// up pinning a stranger that a later failure would then SIGKILL.
    #[test]
    fn test_peer_pidfd_is_taken_atomically_from_the_socket() {
        require_peer_pidfd_support()
            .expect("SO_PEERPIDFD must be available; the server refuses to start without it");

        let (a, _b) = std::os::unix::net::UnixStream::pair().unwrap();
        let pidfd = peer_pidfd(a.as_raw_fd()).expect("SO_PEERPIDFD on a connected socket");
        assert_eq!(
            pidfd_pid(&pidfd).unwrap(),
            std::process::id(),
            "the pidfd must name the peer of THIS socket"
        );

        // An unconnected socket has no peer: the failure must surface, never be papered over
        // with a PID guess.
        let lonely = std::os::unix::net::UnixDatagram::unbound().unwrap();
        assert!(
            peer_pidfd(lonely.as_raw_fd()).is_err(),
            "a socket with no peer must not yield a pidfd"
        );
    }

    /// The kill must land on the process we accepted, and must NEVER land on a stranger
    /// that later inherits the same PID — the pidfd is what guarantees that.
    #[test]
    fn test_peer_vmm_kill_targets_only_the_pinned_process() {
        let mut victim = spawn_victim();
        let peer = PeerVmm::from_pid(victim.id()).expect("pinning a live process");
        assert!(peer.is_alive());

        assert!(
            peer.kill_now("test: fault handler failed"),
            "SIGKILL must be delivered"
        );
        let status = victim.wait().expect("reaping the victim");
        assert!(
            killed_by_sigkill(status),
            "the VMM must die by SIGKILL, got {status:?}"
        );

        // The process is reaped: its PID is now free for the OS to hand to anything. A
        // second kill through the pidfd must report "no such process" instead of signalling
        // whatever holds that number now.
        assert!(!peer.is_alive());
        assert!(
            !peer.kill_now("test: second attempt after the process was reaped"),
            "a pidfd for a reaped process must refuse to signal, not hit a recycled PID"
        );
    }

    /// The headline regression: when the page-fault handler fails, the clone's VMM is
    /// KILLED. Before this, the handler merely logged that the VM "should be killed" and
    /// left it running on zero-filled memory.
    ///
    /// The failure injected is a real one from `drain_events`: a fault at an address the
    /// handshake's mappings do not cover, which is unresolvable by construction.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_handler_failure_kills_the_clones_vmm() {
        use std::io::Write;

        // A snapshot to serve from (contents are irrelevant — the fault never resolves).
        let (snap_path, mem_size) = write_test_snapshot();
        let mem_file = File::open(&snap_path).unwrap();
        let mmap = unsafe { MmapOptions::new().len(mem_size).map(&mem_file).unwrap() };
        let source = Arc::new(PageSource::Copy { mmap });
        std::fs::remove_file(&snap_path).ok();

        // The clone's VMM: a real process the server can kill.
        let victim = spawn_victim();
        let peer = PeerVmm::from_pid(victim.id()).unwrap();

        // Stand in for the clone's guest memory + its userfaultfd.
        let guest = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                4096,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(guest, libc::MAP_FAILED, "mmapping stand-in guest memory");
        let guest_addr = guest as usize;
        let uffd = userfaultfd::UffdBuilder::new()
            .close_on_exec(true)
            .non_blocking(false)
            .user_mode_only(true)
            .create()
            .expect("creating userfaultfd (via /dev/userfaultfd)");
        uffd.register(guest, 4096).expect("MISSING registration");

        let (server_side, client_side) = UnixStream::pair().unwrap();

        // Declare a region that does NOT contain `guest_addr`. The first fault is then
        // unmappable and `drain_events` fails — the same shape as a corrupt handshake or a
        // Firecracker/fcvm disagreement about the guest layout in production.
        let bogus_base = guest_addr.wrapping_add(1 << 30);
        let mappings = format!(
            r#"[{{"base_host_virt_addr": {bogus_base}, "size": 4096, "offset": 0, "page_size": 4096}}]"#
        );

        let client = client_side.into_std().unwrap();
        client.set_nonblocking(false).unwrap();
        client
            .send_with_fd(mappings.as_bytes(), uffd.as_raw_fd())
            .expect("sending mappings + uffd the way Firecracker does");
        // Drop OUR uffd handle: the server's received copy is the only thing keeping the
        // context alive, exactly as in production where Firecracker closes its own copy.
        // When the handler drops it, the pending fault resolves with a zero page — which is
        // precisely the corruption this whole mechanism exists to stop a guest from using.
        drop(uffd);
        std::io::stdout().flush().ok();

        // Touch the guest page: blocks in the kernel until the fault is resolved.
        let toucher =
            std::thread::spawn(move || unsafe { std::ptr::read_volatile(guest_addr as *const u8) });

        // The production fail-closed path, verbatim.
        serve_clone_fail_closed("vm-test", server_side, source, peer).await;

        assert_vmm_was_killed(victim).await;

        // The fault resolved as a zero page once the uffd closed — the corruption the kill
        // prevents the guest from acting on.
        assert_eq!(toucher.join().unwrap(), 0);
        unsafe { libc::munmap(guest, 4096) };
        drop(client);
    }

    #[test]
    fn test_max_clones_default_and_overrides() {
        assert_eq!(
            parse_max_clones(None).unwrap(),
            DEFAULT_MAX_CLONES_PER_SERVER
        );
        assert_eq!(parse_max_clones(Some("8")).unwrap(), 8);
        assert_eq!(parse_max_clones(Some(" 8 ")).unwrap(), 8);

        // A bound that cannot be honoured must fail loudly rather than silently reverting
        // to "unbounded", which is the condition the cap exists to prevent.
        let err = parse_max_clones(Some("0")).unwrap_err().to_string();
        assert!(err.contains("at least 1"), "unexpected error: {err}");
        let err = parse_max_clones(Some("lots")).unwrap_err().to_string();
        assert!(
            err.contains("not a non-negative integer"),
            "unexpected: {err}"
        );
        assert!(parse_max_clones(Some("-1")).is_err());
    }

    /// Two servers must never name the same socket: the old scheme let one server unlink
    /// the socket another was still accepting on (both on startup and on drop).
    #[test]
    fn test_socket_path_is_unique_per_server_instance() {
        let dir = Path::new("/data");
        let a = UffdServer::socket_path_for(dir, "snap", 42, 1000);
        let b = UffdServer::socket_path_for(dir, "snap", 42, 2000); // same PID, reused
        let c = UffdServer::socket_path_for(dir, "snap", 43, 1000); // same start, other PID
        assert_ne!(a, b, "a recycled PID must not reuse the socket name");
        assert_ne!(a, c);
        assert_eq!(
            a,
            UffdServer::socket_path_for(dir, "snap", 42, 1000),
            "the name must be reconstructible: clones derive it from the serve state file"
        );
        assert_eq!(a, Path::new("/data/uffd-snap-42-1000.sock"));
    }
}
