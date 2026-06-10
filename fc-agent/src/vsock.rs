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
    /// Creates a blocking socket, connects (instant for vsock), then sets
    /// non-blocking for use with tokio's AsyncFd.
    pub fn connect(cid: u32, port: u32) -> Result<Self> {
        use nix::sys::socket::{connect, socket, AddressFamily, SockFlag, SockType, VsockAddr};

        // Create blocking socket, connect (instant for same-machine vsock),
        // then switch to non-blocking for AsyncFd.
        let fd = socket(
            AddressFamily::Vsock,
            SockType::Stream,
            SockFlag::empty(),
            None,
        )
        .context("creating vsock socket")?;
        let addr = VsockAddr::new(cid, port);
        connect(fd.as_raw_fd(), &addr).context("connecting vsock")?;

        // Set non-blocking for AsyncFd
        nix::fcntl::fcntl(
            fd.as_raw_fd(),
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
/// be fast and must never block (vsock connect is local/instant).
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
