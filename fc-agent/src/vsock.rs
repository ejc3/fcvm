use anyhow::{bail, Context, Result};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use tokio::io::unix::AsyncFd;

pub const HOST_CID: u32 = 2;
pub const STATUS_PORT: u32 = 4999;
pub const EXEC_PORT: u32 = 4998;
pub const OUTPUT_PORT: u32 = 4997;

/// Async vsock stream — wraps an OwnedFd in AsyncFd for non-blocking I/O.
pub struct VsockStream {
    inner: AsyncFd<OwnedFd>,
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

        let inner = AsyncFd::new(fd).context("wrapping vsock in AsyncFd")?;
        Ok(Self { inner })
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

    /// Accept a connection. Returns a blocking OwnedFd for spawn_blocking handlers.
    pub async fn accept(&self) -> Result<OwnedFd> {
        loop {
            let mut guard = self.inner.readable().await?;
            let client_fd = unsafe {
                libc::accept4(
                    self.inner.get_ref().as_raw_fd(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    libc::SOCK_CLOEXEC,
                )
            };
            if client_fd < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::WouldBlock {
                    guard.clear_ready();
                    continue;
                }
                bail!("accept failed: {}", err);
            }
            return Ok(unsafe { OwnedFd::from_raw_fd(client_fd) });
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
pub fn notify_container_exit(exit_code: i32) {
    let msg = format!("exit:{}\n", exit_code);
    if send_status(msg.as_bytes()) {
        eprintln!(
            "[fc-agent] notified host of exit code {} via vsock",
            exit_code
        );
    } else {
        eprintln!("[fc-agent] WARNING: failed to send exit status to host");
    }
}

/// Notify host that the container has started.
pub fn notify_container_started() {
    if send_status(b"ready\n") {
        eprintln!("[fc-agent] container started, notified host via vsock");
    } else {
        eprintln!("[fc-agent] WARNING: failed to send ready status to host");
    }
}
