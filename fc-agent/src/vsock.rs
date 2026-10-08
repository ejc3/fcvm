use anyhow::{bail, Context, Result};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{ready, Poll};
use tokio::io::unix::AsyncFd;
use tokio::io::Interest;

pub const HOST_CID: u32 = 2;
pub const STATUS_PORT: u32 = 4999;
pub const EXEC_PORT: u32 = 4998;
pub const OUTPUT_PORT: u32 = 4997;
pub const RESTORE_COMPLETE_PORT: u32 = 4994;
pub const EGRESS_PROXY_PORT: u32 = 52000;

/// Implement AsyncRead for a type with `inner: Arc<AsyncFd<OwnedFd>>`.
macro_rules! impl_async_read {
    ($Type:ty) => {
        impl tokio::io::AsyncRead for $Type {
            fn poll_read(
                self: Pin<&mut Self>,
                cx: &mut std::task::Context<'_>,
                buf: &mut tokio::io::ReadBuf<'_>,
            ) -> Poll<std::io::Result<()>> {
                loop {
                    let mut guard = ready!(self.inner.poll_read_ready(cx))?;
                    match guard.try_io(|inner| {
                        let n = unsafe {
                            libc::read(
                                inner.as_raw_fd(),
                                buf.unfilled_mut().as_mut_ptr().cast(),
                                buf.remaining(),
                            )
                        };
                        if n < 0 {
                            Err(std::io::Error::last_os_error())
                        } else {
                            unsafe { buf.assume_init(n as usize) };
                            buf.advance(n as usize);
                            Ok(())
                        }
                    }) {
                        Ok(result) => return Poll::Ready(result),
                        Err(_would_block) => continue,
                    }
                }
            }
        }
    };
}

/// Implement AsyncWrite for a type with `inner: Arc<AsyncFd<OwnedFd>>`.
macro_rules! impl_async_write {
    ($Type:ty) => {
        impl tokio::io::AsyncWrite for $Type {
            fn poll_write(
                self: Pin<&mut Self>,
                cx: &mut std::task::Context<'_>,
                buf: &[u8],
            ) -> Poll<std::io::Result<usize>> {
                loop {
                    let mut guard = ready!(self.inner.poll_write_ready(cx))?;
                    match guard.try_io(|inner| {
                        let n = unsafe {
                            libc::write(inner.as_raw_fd(), buf.as_ptr().cast(), buf.len())
                        };
                        if n < 0 {
                            Err(std::io::Error::last_os_error())
                        } else {
                            Ok(n as usize)
                        }
                    }) {
                        Ok(result) => return Poll::Ready(result),
                        Err(_would_block) => continue,
                    }
                }
            }

            fn poll_flush(
                self: Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
            ) -> Poll<std::io::Result<()>> {
                Poll::Ready(Ok(()))
            }

            fn poll_shutdown(
                self: Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
            ) -> Poll<std::io::Result<()>> {
                unsafe { libc::shutdown(self.inner.get_ref().as_raw_fd(), libc::SHUT_WR) };
                Poll::Ready(Ok(()))
            }
        }
    };
}

/// `SO_VM_SOCKETS_CONNECT_TIMEOUT` from linux/vm_sockets.h, which libc does not define.
/// Its value is a struct timeval: 6 is the header's `SO_VM_SOCKETS_CONNECT_TIMEOUT_OLD`, whose
/// timeval is two longs on the 64-bit targets fc-agent builds for.
const SO_VM_SOCKETS_CONNECT_TIMEOUT: libc::c_int = 6;
const _: () = assert!(
    std::mem::size_of::<libc::timeval>() == 2 * std::mem::size_of::<libc::c_long>(),
    "SO_VM_SOCKETS_CONNECT_TIMEOUT_OLD takes a timeval of two longs"
);

/// Connect a blocking vsock socket. A blocking vsock connect waits for the host's answer
/// (Firecracker connects to the host's listener, then answers the guest) for at most
/// Linux's default of 2 s, and blocks the calling thread meanwhile. Every other fc-agent
/// connection keeps that default. The restore ACK, which fails closed, uses
/// [`connect_blocking_within`] on a blocking thread instead (#1080).
pub fn connect_blocking(cid: u32, port: u32) -> Result<OwnedFd> {
    let fd = vsock_socket(None)?;
    connect_vsock(&fd, cid, port).context("connecting vsock")?;
    Ok(fd)
}

/// Connect a blocking vsock socket, waiting up to `timeout` for the host's answer, and
/// return it with how long its connect syscalls blocked. Blocks the calling thread for up
/// to that long, so call it from a blocking task. A signal that interrupts the wait closes
/// the socket (Linux does not restart a vsock connect that has a finite timeout), so the
/// connect starts over on a new socket with the time left. The retry is meaningful for the
/// restore ACK: the host keeps accepting connections, so the first (closed) socket is
/// abandoned and the retry's socket is the one it reads (`receive_restore_completion`,
/// src/commands/podman/listeners.rs).
///
/// The syscall time is measured around each connect syscall on this thread, so it leaves
/// out the wait for a thread to run this and for the caller to be scheduled again. A failed
/// connect's error carries it too.
pub fn connect_blocking_within(
    cid: u32,
    port: u32,
    timeout: std::time::Duration,
) -> Result<(OwnedFd, std::time::Duration)> {
    anyhow::ensure!(
        !timeout.is_zero(),
        "a vsock connect timeout must be longer than zero"
    );
    let deadline = std::time::Instant::now() + timeout;
    let mut blocked = std::time::Duration::ZERO;
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        anyhow::ensure!(
            !left.is_zero(),
            "connecting vsock: interrupted by signals until the {timeout:?} deadline passed \
             (connect syscall time {} ms)",
            blocked.as_millis()
        );
        let fd = vsock_socket(Some(left))?;
        let attempt = std::time::Instant::now();
        let connected = connect_vsock(&fd, cid, port);
        blocked += attempt.elapsed();
        match connected {
            Ok(()) => return Ok((fd, blocked)),
            Err(nix::errno::Errno::EINTR) => continue,
            Err(errno) => {
                return Err(errno).with_context(|| {
                    format!(
                        "connecting vsock (connect syscall time {} ms)",
                        blocked.as_millis()
                    )
                })
            }
        }
    }
}

/// A blocking vsock socket. With `connect_timeout`, its connect waits that long for the
/// host's answer instead of Linux's 2 s default.
fn vsock_socket(connect_timeout: Option<std::time::Duration>) -> Result<OwnedFd> {
    use nix::sys::socket::{socket, AddressFamily, SockFlag, SockType};

    let fd = socket(
        AddressFamily::Vsock,
        SockType::Stream,
        SockFlag::SOCK_CLOEXEC,
        None,
    )
    .context("creating vsock socket")?;
    if let Some(timeout) = connect_timeout {
        let value = libc::timeval {
            tv_sec: timeout.as_secs().try_into().unwrap_or(libc::c_long::MAX),
            tv_usec: timeout.subsec_micros() as _,
        };
        // SAFETY: `fd` is an open socket, and `value` is a timeval that outlives the call.
        let rc = unsafe {
            libc::setsockopt(
                fd.as_raw_fd(),
                libc::AF_VSOCK,
                SO_VM_SOCKETS_CONNECT_TIMEOUT,
                (&value as *const libc::timeval).cast(),
                std::mem::size_of::<libc::timeval>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error())
                .context("setting the vsock connect timeout");
        }
    }
    Ok(fd)
}

fn connect_vsock(fd: &OwnedFd, cid: u32, port: u32) -> nix::Result<()> {
    use nix::sys::socket::{connect, VsockAddr};

    connect(fd.as_raw_fd(), &VsockAddr::new(cid, port))
}

/// Async vsock stream — wraps an OwnedFd in Arc<AsyncFd> for non-blocking I/O.
///
/// Uses Arc internally so the fd can be shared between read/write halves
/// (via `split()`) and an error watcher (via `wait_for_error()`). This enables
/// the egress proxy to detect vsock transport reset (EPOLLERR) natively via
/// tokio's Interest::ERROR, without external Notify signals.
pub struct VsockStream {
    inner: Arc<AsyncFd<OwnedFd>>,
}

impl VsockStream {
    /// Connect to the host on the given vsock port.
    ///
    /// Creates a blocking socket and connects, waiting up to Linux's 2 s default for the
    /// host's response ([`connect_blocking`]), then sets non-blocking for use with tokio's
    /// AsyncFd.
    pub fn connect(cid: u32, port: u32) -> Result<Self> {
        Self::from_connected(connect_blocking(cid, port)?)
    }

    /// Wrap a connected blocking vsock socket for async I/O.
    pub fn from_connected(fd: OwnedFd) -> Result<Self> {
        // Set non-blocking for AsyncFd
        nix::fcntl::fcntl(
            &fd,
            nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK),
        )
        .context("setting O_NONBLOCK on vsock")?;

        let inner = Arc::new(AsyncFd::new(fd).context("wrapping vsock in AsyncFd")?);
        Ok(Self { inner })
    }

    /// Split into read and write halves for concurrent use.
    ///
    /// The original VsockStream remains valid after split — use `wait_for_error()`
    /// on it to detect vsock transport reset while the halves are in use.
    pub fn split(&self) -> (VsockReadHalf, VsockWriteHalf) {
        (
            VsockReadHalf {
                inner: self.inner.clone(),
            },
            VsockWriteHalf {
                inner: self.inner.clone(),
            },
        )
    }

    /// Wait for EPOLLERR on this fd (vsock transport reset after snapshot restore).
    ///
    /// After VIRTIO_VSOCK_EVENT_TRANSPORT_RESET, the kernel sets EPOLLERR on all
    /// vsock fds. Tokio's `poll_read_ready`/`poll_write_ready` miss this because
    /// `Direction::Read.mask()` = `READABLE | READ_CLOSED` (no ERROR bit), so tasks
    /// blocked in AsyncRead::poll_read are never woken. But `AsyncFd::ready()` with
    /// `Interest::ERROR` detects it natively — the readiness intersection check in
    /// tokio's `Readiness` future matches the stored ERROR state.
    pub async fn wait_for_error(&self) -> std::io::Result<()> {
        let _guard = self.inner.ready(Interest::ERROR).await?;
        Ok(())
    }

    /// Async write_all — waits for writability via epoll, then writes.
    pub async fn write_all(&self, buf: &[u8]) -> std::io::Result<()> {
        let mut pos = 0;
        while pos < buf.len() {
            let mut guard = self.inner.writable().await?;
            match guard.try_io(|inner| {
                let n = unsafe {
                    libc::write(
                        inner.as_raw_fd(),
                        buf[pos..].as_ptr().cast(),
                        buf.len() - pos,
                    )
                };
                if n < 0 {
                    Err(std::io::Error::last_os_error())
                } else if n == 0 {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "write returned 0",
                    ))
                } else {
                    Ok(n as usize)
                }
            }) {
                Ok(Ok(n)) => pos += n,
                Ok(Err(e)) => return Err(e),
                Err(_would_block) => continue,
            }
        }
        Ok(())
    }
}

impl_async_read!(VsockStream);
impl_async_write!(VsockStream);

/// Read half of a VsockStream, produced by `VsockStream::split()`.
pub struct VsockReadHalf {
    inner: Arc<AsyncFd<OwnedFd>>,
}

impl_async_read!(VsockReadHalf);

/// Write half of a VsockStream, produced by `VsockStream::split()`.
pub struct VsockWriteHalf {
    inner: Arc<AsyncFd<OwnedFd>>,
}

impl_async_write!(VsockWriteHalf);

/// Async byte stream over any pollable fd: an exec connection or a PTY master.
///
/// Handles share one fd, so one task can read while another writes.
pub struct AsyncFdStream {
    inner: Arc<AsyncFd<OwnedFd>>,
}

impl AsyncFdStream {
    /// Take ownership of `fd`, switch it to non-blocking and register it with tokio.
    pub fn new(fd: OwnedFd) -> std::io::Result<Self> {
        let flags = nix::fcntl::fcntl(&fd, nix::fcntl::FcntlArg::F_GETFL)?;
        let flags = nix::fcntl::OFlag::from_bits_retain(flags) | nix::fcntl::OFlag::O_NONBLOCK;
        nix::fcntl::fcntl(&fd, nix::fcntl::FcntlArg::F_SETFL(flags))?;
        Ok(Self {
            inner: Arc::new(AsyncFd::new(fd)?),
        })
    }

    /// Another handle on the same fd. The fd closes when the last handle drops.
    pub fn handle(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl AsRawFd for AsyncFdStream {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.inner.get_ref().as_raw_fd()
    }
}

impl_async_read!(AsyncFdStream);
impl_async_write!(AsyncFdStream);

/// Async vsock listener for accept loops (exec server).
pub struct VsockListener {
    inner: AsyncFd<OwnedFd>,
}

impl VsockListener {
    /// Bind and listen on the given vsock port.
    pub fn bind(port: u32) -> Result<Self> {
        use nix::sys::socket::{
            bind, listen, socket, AddressFamily, SockFlag, SockType, VsockAddr,
        };

        let fd = socket(
            AddressFamily::Vsock,
            SockType::Stream,
            SockFlag::SOCK_NONBLOCK,
            None,
        )
        .context("creating vsock listener socket")?;

        bind(fd.as_raw_fd(), &VsockAddr::new(libc::VMADDR_CID_ANY, port))
            .context("binding vsock listener")?;
        listen(
            &fd,
            nix::sys::socket::Backlog::new(128).unwrap_or(nix::sys::socket::Backlog::MAXCONN),
        )
        .context("listening on vsock")?;

        let inner = AsyncFd::new(fd).context("wrapping listener in AsyncFd")?;
        Ok(Self { inner })
    }

    /// Re-register with epoll after vsock transport reset.
    ///
    /// After snapshot restore, the AsyncFd's epoll registration becomes stale —
    /// accept() hangs because tokio never delivers readability events. This method
    /// extracts the socket fd (deregistering from epoll) and re-wraps it in a new
    /// AsyncFd (re-registering with epoll), without closing or rebinding the socket.
    ///
    /// This is preferred over drop+rebind because active connections from before the
    /// snapshot keep the port bound, causing bind() to fail with EADDRINUSE.
    pub fn re_register(self) -> Result<Self> {
        let fd = self.inner.into_inner();
        let inner = AsyncFd::new(fd).context("re-registering listener with AsyncFd")?;
        Ok(Self { inner })
    }

    /// Accept a connection. Returns a blocking OwnedFd for spawn_blocking handlers.
    ///
    /// Robust against lost readiness edges: a vsock connection that is delivered
    /// while the VM is PAUSED (snapshot create: pause → dump → resume) can leave the
    /// accept queue non-empty without ever producing an EPOLLIN edge for the
    /// listener after resume (#617 — host exec CONNECT is ACKed by Firecracker, but
    /// the guest's `readable().await` never wakes; the create path, unlike restore,
    /// never re-registers the listener). Waiting on readiness alone therefore hangs
    /// forever. The fallback: every 2s of idle waiting, optimistically try a
    /// non-blocking accept4 — a queued-but-edgeless connection is then served with
    /// at most 2s latency, while the common path (readiness edge) is unchanged and
    /// the idle cost is one EAGAIN syscall per tick.
    pub async fn accept(&self) -> Result<OwnedFd> {
        loop {
            // Optimistic non-blocking accept first: serves connections whose edge
            // was consumed by a previous cycle (readiness persists until EAGAIN
            // clears it) and connections whose edge was lost across pause/resume.
            let client_fd = unsafe {
                libc::accept4(
                    self.inner.get_ref().as_raw_fd(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    libc::SOCK_CLOEXEC,
                )
            };
            if client_fd >= 0 {
                return Ok(unsafe { OwnedFd::from_raw_fd(client_fd) });
            }
            let err = std::io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::EAGAIN) => {} // queue empty — fall through to wait
                // Transient per-connection / signal conditions: retry without
                // failing the listener (the caller's accept loop would otherwise
                // log-and-retry with no await point — a hot loop under sustained
                // EMFILE pressure).
                Some(libc::EINTR) | Some(libc::ECONNABORTED) => continue,
                Some(libc::EMFILE) | Some(libc::ENFILE) => {
                    eprintln!("[fc-agent] accept: fd exhaustion ({err}); retrying in 100ms");
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    continue;
                }
                _ => bail!("accept failed: {}", err),
            }

            // Queue empty — wait for readiness, with the periodic re-poll fallback.
            match tokio::time::timeout(std::time::Duration::from_secs(2), self.inner.readable())
                .await
            {
                Ok(guard_result) => {
                    let mut guard = guard_result?;
                    // Clear and loop: the accept4 at the top of the loop consumes
                    // the connection(s); readiness re-arms on the next edge, and
                    // the EAGAIN path above re-clears if this edge was stale.
                    guard.clear_ready();
                }
                Err(_) => {
                    // Tick: no edge observed — loop to the optimistic accept4,
                    // which catches a lost-edge connection (#617).
                }
            }
        }
    }
}

/// Send a one-shot message to host on STATUS_PORT.
/// Creates a new connection each time — used for infrequent notifications.
pub fn send_status(message: &[u8]) -> bool {
    use nix::sys::socket::{connect, socket, AddressFamily, SockFlag, SockType, VsockAddr};

    let fd = match socket(
        AddressFamily::Vsock,
        SockType::Stream,
        SockFlag::empty(),
        None,
    ) {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("[fc-agent] WARNING: failed to create vsock socket: {}", e);
            return false;
        }
    };

    if let Err(e) = connect(fd.as_raw_fd(), &VsockAddr::new(HOST_CID, STATUS_PORT)) {
        eprintln!("[fc-agent] WARNING: failed to connect vsock: {}", e);
        return false;
    }

    let written = unsafe { libc::write(fd.as_raw_fd(), message.as_ptr().cast(), message.len()) };
    // fd closed automatically by OwnedFd Drop
    written == message.len() as isize
}

/// Acknowledge one exact restore generation after every guest-side restore phase
/// and the shared Succeeded transition have completed.
///
/// This deliberately uses its own connection rather than the output or status
/// transports: the host binds the matching listener before resume. The host keeps
/// accepting until a connection sends a frame, so a connect that is interrupted and
/// retried on a new socket is still answered (see [`connect_blocking_within`]); it treats
/// any malformed/wrong-generation frame, and a budget with only empty connections, as a
/// restore failure.
///
/// `phases` rides after the epoch, separated by one space, as compact JSON: the per-phase
/// restore timings and how long the ACK's connection took to be answered. The host logs it
/// verbatim so every clone's restore critical path is attributed without guest console
/// capture. It is telemetry, never identity; the host validates only the epoch, and
/// [`exec_proto::restore_complete_frame`] drops telemetry that would overflow the shared
/// frame budget so advisory data can never cost the clone its ACK.
///
/// The connect and write deadlines are shared with the host, whose read of the ACK has to
/// outlast both (#1080).
pub async fn notify_restore_complete(
    restore_epoch: &str,
    phases: &crate::restore::RestorePhases,
) -> Result<()> {
    use exec_proto::{RESTORE_COMPLETE_CONNECT_TIMEOUT, RESTORE_COMPLETE_WRITE_TIMEOUT};

    let started = std::time::Instant::now();
    let (fd, connect_syscall) = tokio::task::spawn_blocking(|| {
        // Test-only: a `burn` guest failpoint here keeps every SCHED_OTHER task, the kernel
        // worker that processes this connect's RESPONSE included, off every CPU, so the
        // connect cannot complete until the burn ends. That is how a VM test measures a
        // vsock connect completing above Linux's 2 s default (#1080). It is zero-cost
        // unless armed. The burn raises this blocking thread above its burners so it can
        // issue the connect, and ends at its deadline. The guard drops at the end of this
        // closure, on this thread, and waits for that deadline before restoring what the
        // burn changed. Nothing may allocate or print between the hit and the connect (see
        // `failpoint::hit_scoped`), and `connect_blocking_within` does neither before its
        // connect syscall returns, unless creating or configuring the socket fails.
        let _burn = failpoint::hit_scoped("restore.pre_ack_connect");
        connect_blocking_within(HOST_CID, RESTORE_COMPLETE_PORT, RESTORE_COMPLETE_CONNECT_TIMEOUT)
    })
    .await
    .context("joining the restore-completion ACK connect")?
    .with_context(|| {
        format!(
            "restore-completion ACK failed (phase=connect expected_epoch={restore_epoch} observed_epoch=<none> waited_ms={})",
            started.elapsed().as_millis()
        )
    })?;
    let connect_ms = started.elapsed().as_secs_f64() * 1000.0;
    // Logged on success too, so the connect's duration under restore load is measured. The
    // syscall time is the connect alone, timed on the connecting thread; `connect_ms` also
    // holds the wait for a blocking thread and for this task to be polled again. The
    // telemetry carries `connect_ms`, where a host log filter on guest console lines cannot
    // drop it.
    eprintln!(
        "[fc-agent] restore-completion ACK connect syscall took {:.0} ms",
        connect_syscall.as_secs_f64() * 1000.0
    );
    eprintln!("[fc-agent] restore-completion ACK connected in {connect_ms:.0} ms");
    let stream = VsockStream::from_connected(fd).with_context(|| {
        format!("restore-completion ACK failed (phase=wrap-socket expected_epoch={restore_epoch})")
    })?;
    let mut phases = phases.clone();
    phases.ack_connect_ms = connect_ms;
    let frame = exec_proto::restore_complete_frame(restore_epoch, &phases.to_frame_json());
    // Bounded: a host that accepts and then stops reading (or a wedged
    // transport) would otherwise block this write forever, leaving the clone
    // alive, unpublished and silent. The caller treats an ACK error as fatal
    // and shuts the clone down, so a deadline converts a silent hang into a
    // diagnosable failure. The frame is a few hundred bytes at most.
    tokio::time::timeout(
        RESTORE_COMPLETE_WRITE_TIMEOUT,
        stream.write_all(frame.as_bytes()),
    )
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "restore-completion ACK failed (phase=write-frame expected_epoch={restore_epoch} reason=timed out after {:?})",
            RESTORE_COMPLETE_WRITE_TIMEOUT
        )
    })?
    .with_context(|| {
        format!(
            "restore-completion ACK failed (phase=write-frame expected_epoch={restore_epoch} observed_epoch={restore_epoch})"
        )
    })?;
    Ok(())
}

/// Notify host of container exit status.
///
/// The exit message is the host's only signal of how the container finished — if it
/// is lost, the host treats a missing exit code as success. Retry a bounded number of
/// times (each attempt is a fresh connection) so a transient vsock failure right
/// before shutdown doesn't silently drop a non-zero exit code.
pub fn notify_container_exit(exit_code: i32) {
    const MAX_ATTEMPTS: u32 = 5;
    const RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(200);

    let msg = format!("exit:{}\n", exit_code);
    for attempt in 1..=MAX_ATTEMPTS {
        if send_status(msg.as_bytes()) {
            eprintln!(
                "[fc-agent] notified host of exit code {} via vsock",
                exit_code
            );
            return;
        }
        if attempt < MAX_ATTEMPTS {
            eprintln!(
                "[fc-agent] WARNING: failed to send exit status to host (attempt {}/{}), retrying",
                attempt, MAX_ATTEMPTS
            );
            std::thread::sleep(RETRY_DELAY);
        }
    }
    eprintln!(
        "[fc-agent] WARNING: failed to send exit status to host after {} attempts",
        MAX_ATTEMPTS
    );
}

/// Notify the host that the guest is rebooting (vs powering off).
///
/// Sent by the systemd system-shutdown hook (which runs `fc-agent --notify-reboot`)
/// only when the shutdown verb is "reboot". The host uses this as the positive
/// signal to relaunch Firecracker in place instead of treating the firecracker
/// exit as VM termination — so a guest `reboot` behaves like a disk-only clone
/// cold boot (storage preserved, captured container restarted, identity regenerated).
///
/// Best-effort with a few retries: the hook runs late in shutdown, so the send must
/// be fast: each connect waits at most Linux's 2 s vsock default.
pub fn notify_reboot() -> bool {
    const MAX_ATTEMPTS: u32 = 3;
    const RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(100);
    for attempt in 1..=MAX_ATTEMPTS {
        if send_status(b"reboot\n") {
            eprintln!("[fc-agent] notified host of reboot intent via vsock");
            return true;
        }
        if attempt < MAX_ATTEMPTS {
            std::thread::sleep(RETRY_DELAY);
        }
    }
    eprintln!("[fc-agent] WARNING: failed to send reboot notification to host");
    false
}

/// Notify host that the container has started.
pub fn notify_container_started() {
    if send_status(b"ready\n") {
        eprintln!("[fc-agent] container started, notified host via vsock");
    } else {
        eprintln!("[fc-agent] WARNING: failed to send ready status to host");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A vsock socket made with a connect timeout reports that timeout back. Linux's
    /// default is 2 s (#1080), so a socket that skipped the option would read 2 s here.
    /// Needs AF_VSOCK on the build host (CI runs privileged) and fails loudly without it.
    #[test]
    fn a_vsock_socket_takes_the_connect_timeout_it_is_given() {
        let fd = vsock_socket(Some(std::time::Duration::from_secs(10)))
            .expect("creating a vsock socket with a connect timeout");
        let mut value = libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        };
        let mut len = std::mem::size_of::<libc::timeval>() as libc::socklen_t;
        // SAFETY: `fd` is an open socket; `value` and `len` outlive the call.
        let rc = unsafe {
            libc::getsockopt(
                fd.as_raw_fd(),
                libc::AF_VSOCK,
                SO_VM_SOCKETS_CONNECT_TIMEOUT,
                (&mut value as *mut libc::timeval).cast(),
                &mut len,
            )
        };
        assert_eq!(rc, 0, "getsockopt: {}", std::io::Error::last_os_error());
        assert_eq!((value.tv_sec, value.tv_usec), (10, 0));
    }

    /// A zero timeout would leave the kernel's default in place, so it is refused.
    #[test]
    fn a_zero_connect_timeout_is_refused() {
        let error =
            connect_blocking_within(HOST_CID, RESTORE_COMPLETE_PORT, std::time::Duration::ZERO)
                .expect_err("a zero connect timeout was accepted");
        assert!(
            format!("{error:#}").contains("longer than zero"),
            "unexpected error: {error:#}"
        );
    }

    /// A failed connect says how long its connect syscall blocked, measured on the
    /// connecting thread, which a restore that timed out reports beside the end-to-end wait
    /// (#1080). The test holds a socket bound, not listening, on a local port the kernel
    /// picked, so no other test or process can listen there and the kernel resets the
    /// connect at once. Without vsock loopback nothing can listen on the local CID, so the
    /// test connects to a fixed port instead.
    #[test]
    fn a_failed_connect_reports_its_connect_syscall_time() {
        use nix::sys::socket::{bind, getsockname, VsockAddr};

        // Bound until the test returns. Meanwhile the kernel refuses any other bind of this
        // port on VMADDR_CID_LOCAL or VMADDR_CID_ANY, the only bindings a connect to the
        // local CID can reach. Without vsock loopback the bind fails with EADDRNOTAVAIL
        // (__vsock_bind accepts VMADDR_CID_LOCAL only with the loopback transport), and then
        // nothing can listen on the local CID at all, so any port is refused.
        let reserved = vsock_socket(None).expect("creating a vsock socket");
        let port = match bind(
            reserved.as_raw_fd(),
            &VsockAddr::new(libc::VMADDR_CID_LOCAL, libc::VMADDR_PORT_ANY),
        ) {
            Ok(()) => getsockname::<VsockAddr>(reserved.as_raw_fd())
                .expect("reading back the reserved vsock port")
                .port(),
            Err(nix::errno::Errno::EADDRNOTAVAIL) => 0x7fff_fff0,
            Err(errno) => panic!("binding a local vsock port: {errno}"),
        };

        let error = connect_blocking_within(
            libc::VMADDR_CID_LOCAL,
            port,
            std::time::Duration::from_secs(1),
        )
        .expect_err("a connect to a bound port that does not listen succeeded");
        assert!(
            format!("{error:#}").contains("connect syscall time"),
            "the connect error does not carry the connect syscall time: {error:#}"
        );
    }

    /// The restore-completion ACK connects and writes within the deadlines it shares with
    /// the host, not Linux's 2 s connect default. Only the code above the test module is
    /// searched, and comments are skipped, so neither this test's own text nor a comment
    /// can satisfy it.
    #[test]
    fn the_restore_ack_uses_the_shared_deadlines() {
        let source = include_str!("vsock.rs");
        let code = &source[..source
            .find("\n#[cfg(test)]\nmod tests {")
            .expect("vsock.rs has no test module")];
        let start = code
            .find("pub async fn notify_restore_complete(")
            .expect("notify_restore_complete is gone");
        let end = code[start..]
            .find("\n}\n")
            .expect("notify_restore_complete has no end");
        let squeezed: String = code[start..start + end]
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .flat_map(|line| line.split_whitespace())
            .collect();
        for call in [
            "connect_blocking_within(HOST_CID,RESTORE_COMPLETE_PORT,RESTORE_COMPLETE_CONNECT_TIMEOUT)",
            "tokio::time::timeout(RESTORE_COMPLETE_WRITE_TIMEOUT,",
        ] {
            assert!(squeezed.contains(call), "notify_restore_complete no longer calls {call}");
        }
    }
}
