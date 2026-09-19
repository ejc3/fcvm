//! Host side of a framed exec session.
//!
//! One implementation serves every `fcvm exec` mode and `podman run -it`. The
//! guest sends the command's output as exec-proto frames: stdout (or the PTY
//! stream) and stderr arrive separately and byte-exact, and go to this
//! process's stdout and stderr the way `podman exec` delivers them.

use std::io::Write;
use std::os::unix::io::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};
use nix::sys::termios::{self, SetArg, Termios};
use tracing::{debug, info, warn};

/// Global storage for terminal restoration on signal (async-signal-safe)
/// Using atomics and static mut with careful synchronization to be signal-safe.
/// SAFETY: ORIG_TERMIOS is only written while TERMIOS_SAVED is false,
/// only read in signal handler after TERMIOS_SAVED is true.
static ORIG_FD: AtomicI32 = AtomicI32::new(-1);
static mut ORIG_TERMIOS: Option<Termios> = None;
static TERMIOS_SAVED: AtomicBool = AtomicBool::new(false);

/// Global flag to track if signal handlers are installed
static SIGNAL_HANDLERS_INSTALLED: AtomicBool = AtomicBool::new(false);

/// Write end of the pipe the SIGWINCH handler pokes; -1 while no session wants it.
static WINCH_PIPE_TX: AtomicI32 = AtomicI32::new(-1);

/// SIGWINCH handler: async-signal-safe, it only writes one byte to a pipe.
extern "C" fn winch_handler(_sig: libc::c_int) {
    let fd = WINCH_PIPE_TX.load(Ordering::Acquire);
    if fd >= 0 {
        let byte = 1u8;
        // A full pipe already holds a pending wake-up, so a failed write is fine.
        unsafe { libc::write(fd, (&byte as *const u8).cast(), 1) };
    }
}

/// Delivers the terminal's window changes to the input thread for one session.
struct WinchWatch {
    rx: std::os::fd::OwnedFd,
    /// Held so the handler's fd stays valid until `drop` detaches it.
    _tx: std::os::fd::OwnedFd,
}

impl WinchWatch {
    fn arm() -> Result<Self> {
        let (rx, tx) =
            nix::unistd::pipe2(nix::fcntl::OFlag::O_NONBLOCK | nix::fcntl::OFlag::O_CLOEXEC)
                .context("creating window-change pipe")?;
        WINCH_PIPE_TX.store(tx.as_raw_fd(), Ordering::Release);
        let action = SigAction::new(
            SigHandler::Handler(winch_handler),
            SaFlags::SA_RESTART,
            SigSet::empty(),
        );
        // SAFETY: winch_handler is async-signal-safe.
        unsafe { sigaction(Signal::SIGWINCH, &action) }.context("installing SIGWINCH handler")?;
        Ok(Self { rx, _tx: tx })
    }
}

impl Drop for WinchWatch {
    fn drop(&mut self) {
        // Detach the handler from the pipe before the pipe closes.
        WINCH_PIPE_TX.store(-1, Ordering::Release);
        let default = SigAction::new(SigHandler::SigDfl, SaFlags::empty(), SigSet::empty());
        let _ = unsafe { sigaction(Signal::SIGWINCH, &default) };
    }
}

/// The size of the terminal on `fd`, or `None` when `fd` is not a terminal.
pub(crate) fn terminal_size(fd: i32) -> Option<exec_proto::TtySize> {
    let mut winsize: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut winsize) } != 0 {
        return None;
    }
    Some(exec_proto::TtySize {
        rows: winsize.ws_row,
        cols: winsize.ws_col,
    })
}

/// Install signal handlers for terminal restoration
fn install_signal_handlers() {
    if SIGNAL_HANDLERS_INSTALLED.swap(true, Ordering::SeqCst) {
        return; // Already installed
    }

    // Handler that restores terminal - must be async-signal-safe
    extern "C" fn signal_handler(_sig: libc::c_int) {
        if TERMIOS_SAVED.load(Ordering::Acquire) {
            let fd = ORIG_FD.load(Ordering::Acquire);
            if fd >= 0 {
                // SAFETY: We only read ORIG_TERMIOS after TERMIOS_SAVED is true,
                // and it's only written before TERMIOS_SAVED becomes true.
                unsafe {
                    if let Some(ref termios) = ORIG_TERMIOS {
                        // tcsetattr is async-signal-safe per POSIX
                        let _ = termios::tcsetattr(
                            BorrowedFd::borrow_raw(fd),
                            SetArg::TCSANOW,
                            termios,
                        );
                    }
                }
            }
        }
        // SA_RESETHAND auto-resets handler to SIG_DFL, so signal will terminate process
    }

    // Use sigaction with SA_RESETHAND for well-defined behavior
    let handler = SigHandler::Handler(signal_handler);
    let action = SigAction::new(handler, SaFlags::SA_RESETHAND, SigSet::empty());

    // Install handlers for termination signals
    // SAFETY: signal_handler is async-signal-safe
    unsafe {
        let _ = sigaction(Signal::SIGTERM, &action);
        let _ = sigaction(Signal::SIGQUIT, &action);
        let _ = sigaction(Signal::SIGHUP, &action);
    }
    // Note: SIGINT (Ctrl+C) is passed through to guest in raw mode
}

/// Run a TTY session by listening on a Unix socket.
///
/// Used by `podman run -it` where the host listens and guest connects.
///
/// 1. Binds and accepts connection on Unix socket
/// 2. Delegates to `run_tty_session_connected()` for I/O handling
/// 3. Cleans up socket on exit
pub fn run_tty_session(socket_path: &str, tty: bool, interactive: bool) -> Result<i32> {
    run_tty_session_cancellable(
        socket_path,
        tty,
        interactive,
        tokio_util::sync::CancellationToken::new(),
        None,
    )
}

/// Run a TTY listener that can be stopped before the guest connects.
///
/// Snapshot restore creates its host listeners before starting the VMM.  A
/// restore failure in that window must be able to join the listener thread
/// before deleting its socket directory; a blocking `accept(2)` cannot provide
/// that guarantee.  The ordinary podman path passes a never-cancelled token via
/// [`run_tty_session`], while snapshot setup supplies its teardown token and a
/// readiness channel so it never races cleanup against a listener that has not
/// bound yet.
pub(crate) fn run_tty_session_cancellable(
    socket_path: &str,
    tty: bool,
    interactive: bool,
    cancel: tokio_util::sync::CancellationToken,
    ready: Option<std::sync::mpsc::SyncSender<Result<(), String>>>,
) -> Result<i32> {
    // Remove stale socket if it exists
    let _ = std::fs::remove_file(socket_path);

    let listener = match UnixListener::bind(socket_path)
        .with_context(|| format!("binding to {}", socket_path))
    {
        Ok(listener) => listener,
        Err(error) => {
            if let Some(ready) = ready {
                let _ = ready.send(Err(format!("{error:#}")));
            }
            return Err(error);
        }
    };

    info!(socket = %socket_path, tty, interactive, "TTY session started");

    // A nonblocking accept loop is what makes cancellation an ownership
    // barrier: after the token is cancelled, joining this thread proves it no
    // longer owns the socket or can touch the runtime directory.
    if let Err(error) = listener
        .set_nonblocking(true)
        .context("setting listener to nonblocking")
    {
        if let Some(ready) = ready {
            let _ = ready.send(Err(format!("{error:#}")));
        }
        return Err(error);
    }
    if let Some(ready) = ready {
        let _ = ready.send(Ok(()));
    }
    let stream = loop {
        if cancel.is_cancelled() {
            let _ = std::fs::remove_file(socket_path);
            return Ok(0);
        }
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(error) => return Err(error).context("accepting connection"),
        }
    };

    debug!("TTY connection established");

    // Run the session with the connected stream. This is the `podman run -it`
    // console (host-bound listener socket, not an exec vsock session), so the
    // snapshot-orphan guard does not apply — and the socket carries no read
    // timeout, so the disabled guard is never even consulted.
    let result = run_tty_session_connected(
        stream,
        tty,
        interactive,
        crate::commands::exec::SnapshotOrphanGuard::disabled(),
    );

    // Clean up socket
    let _ = std::fs::remove_file(socket_path);

    result
}

/// Run a session on a pre-connected stream.
///
/// Used by `fcvm exec`, where the host has already connected to the guest and
/// completed the handshake.
///
/// 1. Sets terminal to raw mode if `tty=true` and stdin is a terminal
/// 2. Spawns reader thread (socket -> stdout and stderr)
/// 3. Spawns input thread: stdin -> socket, then end of input, if
///    `interactive=true`; window changes -> socket, if stdin is a terminal
/// 4. Returns the remote command's exit code, or an error when the session
///    ended without one (connection closed, snapshot-pause orphan, protocol
///    error). `fcvm exec` exits 125 for those, as `podman exec` does for its
///    own failures.
///
/// `guard` is the exec session's snapshot-orphan guard: reads that sit idle
/// past the epoch poll interval check it, so a session silently killed by a
/// VM snapshot pause aborts loudly instead of hanging (an idle shell with an
/// unchanged epoch waits forever, which is correct). The `podman run -it`
/// console path passes `SnapshotOrphanGuard::disabled()`.
pub fn run_tty_session_connected(
    stream: UnixStream,
    tty: bool,
    interactive: bool,
    guard: crate::commands::exec::SnapshotOrphanGuard,
) -> Result<i32> {
    // Set up raw terminal mode if TTY requested
    let stdin = std::io::stdin();
    let stdin_fd = stdin.as_fd();

    // Debug: check if stdin is non-blocking
    match nix::fcntl::fcntl(stdin_fd, nix::fcntl::FcntlArg::F_GETFL) {
        Ok(flags) => debug!(
            "run_tty_session_connected: stdin_fd={}, flags=0x{:x}, O_NONBLOCK={}",
            stdin_fd.as_raw_fd(),
            flags,
            nix::fcntl::OFlag::from_bits_retain(flags).contains(nix::fcntl::OFlag::O_NONBLOCK)
        ),
        Err(e) => debug!(
            "run_tty_session_connected: stdin_fd={}, F_GETFL failed: {}",
            stdin_fd.as_raw_fd(),
            e
        ),
    }
    let raw_mode = RawModeGuard::enter(stdin_fd, tty)?;

    prepare_session_stream(&stream)?;

    // Clone stream for reader/writer, and one handle to end the session with
    let session_stream = stream.try_clone().context("cloning stream for shutdown")?;
    let read_stream = stream.try_clone().context("cloning stream for reader")?;
    let mut write_stream = stream;

    // Spawn reader thread: socket -> stdout. The epoch-guarded reader turns
    // idle-read timeouts into snapshot-orphan checks (see exec.rs).
    // Forwarded stdin is flow-controlled by the guest: the reader thread takes
    // its grants, the input thread spends them.
    let window = match interactive {
        true => Some(StdinWindow::new()?),
        false => None,
    };
    let reader_window = window.clone();
    let reader_thread = std::thread::spawn(move || {
        reader_loop(
            crate::commands::exec::EpochGuardedReader::new(read_stream, guard),
            reader_window,
        )
    });

    // With a terminal on stdin, the guest PTY takes its size now and follows
    // every later window change. `fcvm exec` also put the size in its request,
    // so the command never sees an unsized PTY; `podman run -it` has no
    // request and relies on this first frame.
    let stdin_raw = stdin_fd.as_raw_fd();
    let winch = match raw_mode.is_raw() {
        true => Some(WinchWatch::arm()?),
        false => None,
    };
    if let Some(size) = winch.as_ref().and_then(|_| terminal_size(stdin_raw)) {
        let _ = exec_proto::write_resize(&mut write_stream, size);
    }

    // Spawn the input thread: stdin -> socket if interactive, window changes
    // -> socket if there is a terminal. The wake pipe ends it the moment the
    // command exits.
    let (wake_rx, wake_tx) = nix::unistd::pipe().context("creating input wake pipe")?;
    let writer_thread = if interactive || winch.is_some() {
        let winch_rx = match &winch {
            Some(watch) => Some(watch.rx.try_clone().context("cloning window-change pipe")?),
            None => None,
        };
        Some(std::thread::spawn(move || {
            forward_input(
                InputSources {
                    stdin: window.map(|window| (stdin_raw, window)),
                    wake: wake_rx.as_raw_fd(),
                    winch: winch_rx.as_ref().map(|rx| (rx.as_raw_fd(), stdin_raw)),
                },
                &mut write_stream,
            );
        }))
    } else {
        drop(write_stream);
        drop(wake_rx);
        None
    };

    // Wait for the reader: the command's exit code, or why there is none.
    let outcome = reader_thread
        .join()
        .unwrap_or_else(|_| Err(anyhow::anyhow!("exec reader thread panicked")));

    // Restore the terminal before anything else is printed.
    drop(raw_mode);

    stop_input(&session_stream, wake_tx, writer_thread);
    drop(winch);

    // Returned only now, with the terminal restored, so the caller's error
    // message is not printed into a raw-mode terminal.
    outcome
}

/// End the input thread now that the session is over, and wait for it.
///
/// The thread is either in its poll, which the wake pipe ends, or in a socket
/// write that has not finished, for instance because the guest is paused. A
/// session has no write timeout, so only shutting the socket down releases
/// that write.
fn stop_input(
    session_stream: &UnixStream,
    wake_tx: std::os::fd::OwnedFd,
    input_thread: Option<std::thread::JoinHandle<()>>,
) {
    drop(wake_tx);
    let _ = session_stream.shutdown(std::net::Shutdown::Both);
    if let Some(handle) = input_thread {
        let _ = handle.join();
    }
}

/// Settle the socket's options for the session, after the handshake.
///
/// The handshake bounds its writes with a timeout. A session must not: a write
/// that is slow for any reason, a paused guest for one, would time out and
/// silently drop the rest of the input. A blocked write ends when the guest
/// closes the connection, or when the session ends and shuts the socket down.
fn prepare_session_stream(stream: &UnixStream) -> Result<()> {
    stream
        .set_write_timeout(None)
        .context("clearing the handshake's write timeout")
}

/// Holds the terminal in raw mode for a `-t` session and puts it back when
/// dropped, so every way out of the session, errors included, restores it.
struct RawModeGuard<'fd> {
    fd: BorrowedFd<'fd>,
    original: Option<Termios>,
}

impl<'fd> RawModeGuard<'fd> {
    /// Without `tty`, or without a terminal on `fd`, this holds nothing.
    fn enter(fd: BorrowedFd<'fd>, tty: bool) -> Result<Self> {
        let original = if tty { setup_raw_terminal(fd)? } else { None };
        Ok(Self { fd, original })
    }

    fn is_raw(&self) -> bool {
        self.original.is_some()
    }
}

impl Drop for RawModeGuard<'_> {
    fn drop(&mut self) {
        if let Some(original) = self.original.take() {
            restore_terminal(self.fd, original);
        }
    }
}

/// Set up raw terminal mode
fn setup_raw_terminal(fd: BorrowedFd<'_>) -> Result<Option<Termios>> {
    // -t with a pipe or file on stdin: the guest still gets a PTY, and there
    // is no local terminal to switch to raw mode. `podman exec -t` does the same.
    if !nix::unistd::isatty(fd).unwrap_or(false) {
        return Ok(None);
    }

    // Install signal handlers for terminal restoration before modifying terminal
    install_signal_handlers();

    // Ensure stdin is in blocking mode (tokio may have set it non-blocking)
    if let Ok(flags) = nix::fcntl::fcntl(fd, nix::fcntl::FcntlArg::F_GETFL) {
        let oflags = nix::fcntl::OFlag::from_bits_truncate(flags);
        if oflags.contains(nix::fcntl::OFlag::O_NONBLOCK) {
            let new_flags = oflags & !nix::fcntl::OFlag::O_NONBLOCK;
            let _ = nix::fcntl::fcntl(fd, nix::fcntl::FcntlArg::F_SETFL(new_flags));
            debug!("setup_raw_terminal: cleared O_NONBLOCK from stdin");
        }
    }

    // Save original terminal settings
    let orig = termios::tcgetattr(fd).context("Failed to get terminal attributes")?;

    // Store in global for signal handler access (async-signal-safe approach)
    // SAFETY: We only write while TERMIOS_SAVED is false, ensuring no concurrent read
    ORIG_FD.store(fd.as_raw_fd(), Ordering::Release);
    unsafe {
        ORIG_TERMIOS = Some(orig.clone());
    }
    // Memory barrier: ensure writes above are visible before setting flag
    TERMIOS_SAVED.store(true, Ordering::Release);

    // Set raw mode
    let mut raw = orig.clone();
    termios::cfmakeraw(&mut raw);

    if let Err(e) = termios::tcsetattr(fd, SetArg::TCSANOW, &raw) {
        // Clear global on failure
        TERMIOS_SAVED.store(false, Ordering::Release);
        bail!("Failed to set raw terminal mode: {}", e);
    }

    Ok(Some(orig))
}

/// Restore terminal to original settings
fn restore_terminal(fd: BorrowedFd<'_>, orig_termios: Termios) {
    // Clear global first (signal handler won't need to restore anymore)
    TERMIOS_SAVED.store(false, Ordering::Release);

    if let Err(e) = termios::tcsetattr(fd, SetArg::TCSANOW, &orig_termios) {
        warn!("Failed to restore terminal settings: {}", e);
    }
}

/// Reader: the guest's frames to this process's stdout and stderr, until the
/// command's exit code arrives or the session fails.
fn reader_loop<R: std::io::Read>(stream: R, window: Option<Arc<StdinWindow>>) -> Result<i32> {
    // Hold stdout for the session so nothing else interleaves with the
    // command's output, and empty std's buffer once: from here on the bytes go
    // straight to the descriptor.
    // stderr is NOT locked: logging goes through it too, and the input thread
    // logs before it announces end of input. A held lock would block that
    // thread forever whenever debug logging is on.
    let mut stdout_lock = std::io::stdout().lock();
    let _ = stdout_lock.flush();
    reader_loop_to(
        stream,
        &mut FdWriter(libc::STDOUT_FILENO),
        &mut FdWriter(libc::STDERR_FILENO),
        window.is_some(),
        |bytes| {
            if let Some(window) = &window {
                window.grant(bytes);
            }
        },
    )
}

/// The STDIN bytes the guest has said it will take and the host has not yet
/// sent. The guest grants a window and reopens it as the command reads, so the
/// connection never backs up behind a command that ignores its stdin, and the
/// guest can always see this client go away (see exec-proto).
pub(crate) struct StdinWindow {
    available: AtomicUsize,
    /// Wakes the input thread's poll when a grant arrives.
    grant_rx: std::os::fd::OwnedFd,
    grant_tx: std::os::fd::OwnedFd,
}

impl StdinWindow {
    fn new() -> Result<Arc<Self>> {
        let (grant_rx, grant_tx) =
            nix::unistd::pipe2(nix::fcntl::OFlag::O_NONBLOCK | nix::fcntl::OFlag::O_CLOEXEC)
                .context("creating stdin window pipe")?;
        Ok(Arc::new(Self {
            available: AtomicUsize::new(0),
            grant_rx,
            grant_tx,
        }))
    }

    /// The reader thread calls this for every STDIN_WINDOW frame.
    fn grant(&self, bytes: u32) {
        self.available.fetch_add(bytes as usize, Ordering::AcqRel);
        // A full pipe already holds a pending wake-up.
        let byte = 1u8;
        unsafe { libc::write(self.grant_tx.as_raw_fd(), (&byte as *const u8).cast(), 1) };
    }

    fn available(&self) -> usize {
        self.available.load(Ordering::Acquire)
    }

    fn spend(&self, bytes: usize) {
        self.available.fetch_sub(bytes, Ordering::AcqRel);
    }

    fn clear_wakeups(&self) {
        let mut scratch = [0u8; 256];
        unsafe {
            libc::read(
                self.grant_rx.as_raw_fd(),
                scratch.as_mut_ptr().cast(),
                scratch.len(),
            )
        };
    }
}

/// Writes straight to a file descriptor, with nothing buffered in between, so
/// a failed write says exactly how far it got.
///
/// A descriptor can be non-blocking without our doing, when it is shared with a
/// process that set the flag. A full one is then waited for. Dropping the
/// output, or ending the session over it, would both be wrong.
pub(crate) struct FdWriter(pub i32);

impl Write for FdWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        loop {
            let n = unsafe { libc::write(self.0, data.as_ptr().cast(), data.len()) };
            if n >= 0 {
                return Ok(n as usize);
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::WouldBlock {
                return Err(error);
            }
            let mut room = libc::pollfd {
                fd: self.0,
                events: libc::POLLOUT,
                revents: 0,
            };
            unsafe { libc::poll(&mut room, 1, -1) };
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn reader_loop_to<R: std::io::Read>(
    stream: R,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
    expect_window_first: bool,
    mut on_stdin_window: impl FnMut(u32),
) -> Result<i32> {
    // In a session that forwards stdin, fc-agent's first frame is the stdin
    // window. Output ahead of it comes from an fc-agent that predates flow
    // control: it will never open a window, so no input will be forwarded.
    // `fcvm exec` refuses such an agent in its handshake. The `podman run -it`
    // console has no handshake, so this is where it shows, and is said once.
    let awaiting_window = std::cell::Cell::new(expect_window_first);
    let stderr = std::cell::RefCell::new(stderr);
    let notice_an_old_agent = || -> std::io::Result<()> {
        if awaiting_window.replace(false) {
            let mut stderr = stderr.borrow_mut();
            stderr.write_all(OLD_AGENT_NOTICE.as_bytes())?;
            stderr.flush()?;
        }
        Ok(())
    };
    crate::commands::exec::read_exec_frames(
        stream,
        |data| {
            notice_an_old_agent()?;
            stdout.write_all(data).and_then(|()| stdout.flush())
        },
        |data| {
            notice_an_old_agent()?;
            let mut stderr = stderr.borrow_mut();
            stderr.write_all(data).and_then(|()| stderr.flush())
        },
        |bytes| {
            awaiting_window.set(false);
            on_stdin_window(bytes)
        },
    )
}

/// Written as it stands, with its own line ends: the terminal may be in raw mode.
const OLD_AGENT_NOTICE: &str =
    "\r\nfcvm: the guest sent output before opening a stdin window, so its fc-agent \
     predates this fcvm and takes no input from it.\r\n\
     fcvm: restart the VM, or re-create the snapshot it was restored from.\r\n";

/// What the input thread watches.
struct InputSources {
    /// Forwarded to the guest as STDIN frames, within the window the guest has
    /// granted; `None` without -i.
    stdin: Option<(i32, Arc<StdinWindow>)>,
    /// Readable or hung up once the session is over.
    wake: i32,
    /// `(pipe, terminal)`: the pipe is poked on SIGWINCH, the terminal is
    /// asked for its new size.
    winch: Option<(i32, i32)>,
}

/// Input loop: forward stdin and window changes to the guest.
///
/// Returns when the session is over, when the socket fails, or when stdin has
/// ended and there is no terminal left to watch.
fn forward_input(mut sources: InputSources, stream: &mut impl Write) {
    let mut buf = [0u8; exec_proto::IO_CHUNK];
    let mut total = 0usize;
    loop {
        let stdin = sources.stdin.clone();
        let window_open = stdin.as_ref().map_or(0, |(_, window)| window.available());
        // An fd of -1 makes poll skip that entry. stdin is watched only while
        // the guest will take more of it; a grant wakes the poll to look again.
        let mut fds = [
            sources.wake,
            stdin
                .as_ref()
                .map_or(-1, |(fd, _)| if window_open > 0 { *fd } else { -1 }),
            sources.winch.map_or(-1, |(pipe, _)| pipe),
            stdin
                .as_ref()
                .map_or(-1, |(_, window)| window.grant_rx.as_raw_fd()),
        ]
        .map(|fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        });
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if ready < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            debug!(
                "forward_input: poll error: {}",
                std::io::Error::last_os_error()
            );
            return;
        }
        if fds[0].revents != 0 {
            debug!("forward_input: session over after {} bytes", total);
            return;
        }

        if fds[2].revents != 0 {
            if let Some((pipe, terminal)) = sources.winch {
                // Several window changes may have queued; one size query covers
                // them. A single read empties the pipe (the buffer is as large as
                // the pipe) and cannot block, because poll just reported data.
                let _ = unsafe { libc::read(pipe, buf.as_mut_ptr().cast(), buf.len()) };
                if let Some(size) = terminal_size(terminal) {
                    if exec_proto::write_resize(stream, size).is_err() {
                        return;
                    }
                }
            }
        }

        let Some((input_fd, window)) = stdin else {
            continue;
        };
        if fds[3].revents != 0 {
            window.clear_wakeups();
        }
        if fds[1].revents == 0 {
            continue;
        }
        // Never more than the guest will take.
        let want = buf.len().min(window_open);
        let n = unsafe { libc::read(input_fd, buf.as_mut_ptr().cast(), want) };
        if n > 0 {
            total += n as usize;
            if exec_proto::write_stdin(stream, &buf[..n as usize]).is_err() {
                debug!("forward_input: socket write failed after {} bytes", total);
                return;
            }
            window.spend(n as usize);
            continue;
        }
        if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        // End of file, or an unreadable stdin (closed fd 0). Either way there
        // is no more input, and the command must learn that or a reader such
        // as `cat` never finishes.
        debug!(
            "forward_input: end of input after {} bytes (read={})",
            total, n
        );
        let _ = exec_proto::write_stdin_eof(stream);
        sources.stdin = None;
        if sources.winch.is_none() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// stdin with a window far larger than any of these tests sends.
    fn stdin_only(stdin: i32, wake: i32) -> InputSources {
        let window = StdinWindow::new().unwrap();
        window.grant(u32::MAX);
        InputSources {
            stdin: Some((stdin, window)),
            wake,
            winch: None,
        }
    }

    /// Nothing is forwarded without a grant, and never more than was granted.
    /// A host that sent ahead of the window would back the connection up, and
    /// over vsock the guest could then no longer see the host go away.
    #[test]
    fn input_is_forwarded_only_within_the_guests_window() {
        let (input_rx, input_tx) = nix::unistd::pipe().unwrap();
        let (wake_rx, wake_tx) = nix::unistd::pipe().unwrap();
        let (mut guest, mut host) = UnixStream::pair().unwrap();
        nix::unistd::write(&input_tx, b"hello world").unwrap();
        drop(input_tx);

        let window = StdinWindow::new().unwrap();
        let sources = InputSources {
            stdin: Some((input_rx.as_raw_fd(), window.clone())),
            wake: wake_rx.as_raw_fd(),
            winch: None,
        };
        let worker = std::thread::spawn(move || {
            let _input_rx = input_rx;
            forward_input(sources, &mut host);
        });

        // No window yet: input is ready, and none of it may be sent.
        guest
            .set_read_timeout(Some(std::time::Duration::from_millis(300)))
            .unwrap();
        let mut byte = [0u8; 1];
        let early = std::io::Read::read(&mut guest, &mut byte);
        assert!(
            early.is_err(),
            "input was sent before any window: {early:?}"
        );
        guest.set_read_timeout(None).unwrap();

        window.grant(5);
        match exec_proto::Message::read_from(&mut guest).unwrap() {
            exec_proto::Message::Stdin(data) => assert_eq!(data, b"hello"),
            other => panic!("expected the first five bytes, got {other:?}"),
        }
        window.grant(100);
        let mut rest = Vec::new();
        loop {
            match exec_proto::Message::read_from(&mut guest).unwrap() {
                exec_proto::Message::Stdin(data) => rest.extend_from_slice(&data),
                exec_proto::Message::StdinEof => break,
                other => panic!("unexpected frame: {other:?}"),
            }
        }
        assert_eq!(rest, b" world");
        drop(wake_tx);
        worker.join().unwrap();
    }

    fn decode_all(bytes: &[u8]) -> Vec<exec_proto::Message> {
        let mut cursor = Cursor::new(bytes);
        let mut frames = Vec::new();
        while (cursor.position() as usize) < bytes.len() {
            frames.push(exec_proto::Message::read_from(&mut cursor).expect("whole frame"));
        }
        frames
    }

    /// A session that ends without an Exit frame has no exit status to report.
    /// It is fcvm's failure, not the command's, and must not be dressed up as
    /// the command exiting 1.
    #[test]
    fn a_session_cut_short_is_an_error_not_an_exit_code() {
        let mut wire = Vec::new();
        exec_proto::write_data(&mut wire, b"partial").unwrap();
        let mut truncated = wire.clone();
        let mut exit = Vec::new();
        exec_proto::write_exit(&mut exit, 0).unwrap();
        truncated.extend_from_slice(&exit[..exit.len() - 1]);

        let mut guest_error = Vec::new();
        exec_proto::write_error(&mut guest_error, "cannot start").unwrap();

        for stream in [wire, truncated, vec![0x7b, 0x22], guest_error] {
            let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
            let outcome = reader_loop_to(
                Cursor::new(stream.clone()),
                &mut stdout,
                &mut stderr,
                false,
                |_| {},
            );
            assert!(
                outcome.is_err(),
                "stream {stream:?} was reported as an exit status: {outcome:?}"
            );
        }
    }

    /// A writer whose other end is gone, as stdout is in `fcvm exec -- yes | head -1`.
    struct ClosedPipe;

    impl Write for ClosedPipe {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Nobody is reading the output any more, so the session must end. Carrying
    /// on would run the guest command forever with its output thrown away.
    #[test]
    fn a_closed_stdout_ends_the_session() {
        let mut wire = Vec::new();
        exec_proto::write_data(&mut wire, b"y\n").unwrap();
        exec_proto::write_data(&mut wire, b"y\n").unwrap();
        exec_proto::write_exit(&mut wire, 0).unwrap();

        let outcome = reader_loop_to(
            Cursor::new(wire),
            &mut ClosedPipe,
            &mut Vec::new(),
            false,
            |_| {},
        );
        assert!(outcome.is_err(), "a closed stdout was ignored: {outcome:?}");
    }

    /// The handshake bounds its writes with a timeout. A session must not: a
    /// write that is slow for any reason would time out and silently drop the
    /// rest of the input. `podman exec -i` never drops input.
    #[test]
    fn a_session_stream_has_no_write_timeout() {
        let (stream, _peer) = UnixStream::pair().unwrap();
        stream
            .set_write_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        prepare_session_stream(&stream).unwrap();
        assert_eq!(stream.write_timeout().unwrap(), None);
    }

    fn wire(frames: &[exec_proto::Message]) -> Vec<u8> {
        frames.iter().flat_map(|frame| frame.encode()).collect()
    }

    #[test]
    fn output_before_any_stdin_window_is_reported_once() {
        use exec_proto::Message;
        let frames = wire(&[
            Message::Data(b"a".to_vec()),
            Message::Stderr(b"e".to_vec()),
            Message::Data(b"b".to_vec()),
            Message::Exit(0),
        ]);
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        let code = reader_loop_to(Cursor::new(frames), &mut stdout, &mut stderr, true, |_| {});
        assert_eq!(code.unwrap(), 0);
        assert_eq!(stdout, b"ab");
        let stderr = String::from_utf8(stderr).unwrap();
        assert_eq!(
            stderr.matches("predates this fcvm").count(),
            1,
            "{stderr:?}"
        );
        assert!(
            stderr.starts_with("\r\n") && stderr.ends_with("\r\ne"),
            "{stderr:?}"
        );
    }

    #[test]
    fn a_session_that_opens_its_window_first_reports_nothing() {
        use exec_proto::Message;
        let frames = wire(&[
            Message::StdinWindow(4096),
            Message::Data(b"a".to_vec()),
            Message::Stderr(b"e".to_vec()),
            Message::Exit(3),
        ]);
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        let mut granted = 0;
        let code = reader_loop_to(Cursor::new(frames), &mut stdout, &mut stderr, true, |n| {
            granted += n
        });
        assert_eq!((code.unwrap(), granted), (3, 4096));
        assert_eq!(
            (stdout.as_slice(), stderr.as_slice()),
            (&b"a"[..], &b"e"[..])
        );

        // A session that forwards no stdin expects no window.
        let frames = wire(&[Message::Data(b"a".to_vec()), Message::Exit(0)]);
        let mut stderr = Vec::new();
        reader_loop_to(
            Cursor::new(frames),
            &mut Vec::new(),
            &mut stderr,
            false,
            |_| {},
        )
        .unwrap();
        assert_eq!(stderr, b"");
    }

    /// The reader can finish while the input thread sits in a socket write that
    /// has not completed. With no write timeout in a session, only shutting
    /// the socket down releases that write, and the session must not wait for
    /// the thread before it does.
    #[test]
    fn a_blocked_input_write_does_not_hang_the_end_of_the_session() {
        let (host, _guest_never_reads) = UnixStream::pair().unwrap();
        // A small send buffer, so the write below blocks at once whatever this
        // host's default socket buffer is.
        let small: libc::c_int = 4096;
        let rc = unsafe {
            libc::setsockopt(
                host.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                (&small as *const libc::c_int).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        assert_eq!(rc, 0, "SO_SNDBUF");
        let session_stream = host.try_clone().unwrap();
        let (wake_rx, wake_tx) = nix::unistd::pipe().unwrap();
        let (blocked_tx, blocked_rx) = std::sync::mpsc::channel();

        let mut write_stream = host;
        let input_thread = std::thread::spawn(move || {
            let _wake_rx = wake_rx;
            let chunk = [0u8; 64 * 1024];
            let mut sent = 0usize;
            loop {
                // Tell the test once the socket buffer is surely full.
                if sent == 64 {
                    let _ = blocked_tx.send(());
                }
                if exec_proto::write_stdin(&mut write_stream, &chunk).is_err() {
                    return;
                }
                sent += 1;
            }
        });
        // 64 frames of 64 KiB do not fit the buffer set above, so the signal
        // never comes; the wait itself gives the thread time to block in its write.
        assert!(blocked_rx
            .recv_timeout(std::time::Duration::from_millis(500))
            .is_err());

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            stop_input(&session_stream, wake_tx, Some(input_thread));
            let _ = done_tx.send(());
        });
        assert!(
            done_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .is_ok(),
            "the session end waited on an input thread blocked in a write"
        );
    }

    /// A non-blocking, full descriptor is waited for: every byte arrives, in
    /// order, and the session is not ended over it.
    #[test]
    fn output_to_a_full_non_blocking_descriptor_is_not_lost() {
        let (rx, tx) = nix::unistd::pipe2(nix::fcntl::OFlag::O_NONBLOCK).unwrap();
        let reader = std::thread::spawn(move || {
            // Start late, so the writer meets a full pipe first.
            std::thread::sleep(std::time::Duration::from_millis(300));
            let mut got = Vec::new();
            let mut file = std::fs::File::from(rx);
            let mut buf = [0u8; 4096];
            loop {
                match std::io::Read::read(&mut file, &mut buf) {
                    Ok(0) => return got,
                    Ok(n) => got.extend_from_slice(&buf[..n]),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(1))
                    }
                    Err(e) => panic!("read: {e}"),
                }
            }
        });

        // More than any pipe holds, and a pattern that shows reordering.
        let sent: Vec<u8> = (0..2_000_000u32).map(|i| (i % 251) as u8).collect();
        FdWriter(tx.as_raw_fd()).write_all(&sent).unwrap();
        drop(tx);
        assert!(
            reader.join().unwrap() == sent,
            "output was lost or reordered"
        );
    }

    #[test]
    fn reader_keeps_stdout_and_stderr_apart_and_byte_exact() {
        let mut wire = Vec::new();
        exec_proto::write_data(&mut wire, b"out\xff\r\n").unwrap();
        exec_proto::write_stderr(&mut wire, b"err").unwrap();
        exec_proto::write_data(&mut wire, b"more").unwrap();
        exec_proto::write_exit(&mut wire, 9).unwrap();

        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        let code = reader_loop_to(Cursor::new(wire), &mut stdout, &mut stderr, false, |_| {});
        assert_eq!(code.unwrap(), 9);
        assert_eq!(stdout, b"out\xff\r\nmore");
        assert_eq!(stderr, b"err");
    }

    /// The input thread logs through stderr just before it announces end of
    /// input. A reader that held the process-wide stderr lock while waiting
    /// for frames would block that log line forever, and with it the
    /// announcement: `exec -i` would hang whenever debug logging is on.
    #[test]
    fn an_idle_reader_does_not_hold_the_stderr_lock() {
        let (guest, host) = UnixStream::pair().unwrap();
        let reader = std::thread::spawn(move || reader_loop(host, None));
        // Let the reader reach its blocking read.
        std::thread::sleep(std::time::Duration::from_millis(200));

        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            drop(std::io::stderr().lock());
            let _ = locked_tx.send(());
        });
        let free = locked_rx
            .recv_timeout(std::time::Duration::from_secs(3))
            .is_ok();

        // End the session so the reader thread finishes either way.
        let mut guest = guest;
        exec_proto::write_exit(&mut guest, 0).unwrap();
        assert_eq!(reader.join().unwrap().unwrap(), 0);
        assert!(
            free,
            "stderr stayed locked while the reader waited for frames"
        );
    }

    #[test]
    fn input_is_forwarded_and_then_end_of_input_is_announced() {
        let (input_rx, input_tx) = nix::unistd::pipe().unwrap();
        let (wake_rx, _wake_tx) = nix::unistd::pipe().unwrap();
        nix::unistd::write(&input_tx, b"hello").unwrap();
        drop(input_tx);

        let mut wire = Vec::new();
        forward_input(
            stdin_only(input_rx.as_raw_fd(), wake_rx.as_raw_fd()),
            &mut wire,
        );
        let frames = decode_all(&wire);
        assert!(
            matches!(
                frames.as_slice(),
                [exec_proto::Message::Stdin(data), exec_proto::Message::StdinEof] if data == b"hello"
            ),
            "{frames:?}"
        );
    }

    #[test]
    fn an_unreadable_stdin_counts_as_end_of_input() {
        // What `fcvm exec -i <&-` sees: poll reports POLLNVAL and read fails
        // with EBADF. Closing a real fd would not do for the test, because the
        // next pipe reuses its number; this number is never a valid fd.
        const NEVER_OPEN: i32 = i32::MAX - 1;
        let (wake_rx, _wake_tx) = nix::unistd::pipe().unwrap();

        let mut wire = Vec::new();
        forward_input(stdin_only(NEVER_OPEN, wake_rx.as_raw_fd()), &mut wire);
        let frames = decode_all(&wire);
        assert!(
            matches!(frames.as_slice(), [exec_proto::Message::StdinEof]),
            "{frames:?}"
        );
    }

    #[test]
    fn the_input_thread_stops_at_once_when_the_session_ends() {
        // stdin stays open and silent; only the wake pipe ends the loop.
        let (input_rx, _input_tx) = nix::unistd::pipe().unwrap();
        let (wake_rx, wake_tx) = nix::unistd::pipe().unwrap();
        let worker = std::thread::spawn(move || {
            let mut wire = Vec::new();
            forward_input(
                stdin_only(input_rx.as_raw_fd(), wake_rx.as_raw_fd()),
                &mut wire,
            );
            wire
        });
        let started = std::time::Instant::now();
        drop(wake_tx);
        let wire = worker.join().unwrap();
        assert!(started.elapsed() < std::time::Duration::from_millis(500));
        assert!(
            wire.is_empty(),
            "a live stdin must not be reported as ended"
        );
    }

    #[test]
    fn cancellable_listener_exits_before_guest_connects() {
        let dir = tempfile::tempdir().expect("temporary TTY directory");
        let socket = dir.path().join("tty.sock");
        let socket_text = socket.to_string_lossy().into_owned();
        let cancel = tokio_util::sync::CancellationToken::new();
        let worker_cancel = cancel.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);

        let worker = std::thread::spawn(move || {
            let result = run_tty_session_cancellable(
                &socket_text,
                false,
                false,
                worker_cancel,
                Some(ready_tx),
            );
            let _ = done_tx.send(result);
        });

        ready_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("TTY listener did not report readiness")
            .expect("TTY listener failed to bind");
        assert!(socket.exists(), "ready listener did not own its socket");

        cancel.cancel();
        done_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("cancelled TTY listener stayed blocked in accept")
            .expect("cancelled TTY listener returned an error");
        worker.join().expect("TTY listener thread panicked");
        assert!(!socket.exists(), "cancelled TTY listener left its socket");
    }

    #[test]
    fn a_window_change_sends_the_terminals_new_size() {
        let pty = nix::pty::openpty(
            Some(&nix::pty::Winsize {
                ws_row: 33,
                ws_col: 99,
                ws_xpixel: 0,
                ws_ypixel: 0,
            }),
            None,
        )
        .unwrap();
        let (winch_rx, winch_tx) = nix::unistd::pipe().unwrap();
        let (wake_rx, wake_tx) = nix::unistd::pipe().unwrap();
        let (mut guest, mut host) = UnixStream::pair().unwrap();
        let terminal = pty.slave.as_raw_fd();

        let worker = std::thread::spawn(move || {
            forward_input(
                InputSources {
                    stdin: None,
                    wake: wake_rx.as_raw_fd(),
                    winch: Some((winch_rx.as_raw_fd(), terminal)),
                },
                &mut host,
            );
        });
        // Two queued changes collapse into one size query.
        nix::unistd::write(&winch_tx, &[1, 1]).unwrap();
        match exec_proto::Message::read_from(&mut guest).unwrap() {
            exec_proto::Message::Resize(size) => {
                assert_eq!(size, exec_proto::TtySize { rows: 33, cols: 99 })
            }
            other => panic!("expected Resize, got {:?}", other),
        }
        drop(wake_tx);
        worker.join().unwrap();
        drop(pty);

        // Nothing else was sent: the stream ends right after the one frame.
        let mut rest = Vec::new();
        std::io::Read::read_to_end(&mut guest, &mut rest).unwrap();
        assert!(rest.is_empty(), "{rest:?}");
    }

    #[test]
    fn end_of_input_keeps_the_thread_alive_while_a_terminal_is_watched() {
        let pty = nix::pty::openpty(None, None).unwrap();
        let (input_rx, input_tx) = nix::unistd::pipe().unwrap();
        let (winch_rx, _winch_tx) = nix::unistd::pipe().unwrap();
        let (wake_rx, wake_tx) = nix::unistd::pipe().unwrap();
        let (mut guest, mut host) = UnixStream::pair().unwrap();
        let terminal = pty.slave.as_raw_fd();
        drop(input_tx);

        let worker = std::thread::spawn(move || {
            forward_input(
                InputSources {
                    stdin: stdin_only(input_rx.as_raw_fd(), -1).stdin,
                    wake: wake_rx.as_raw_fd(),
                    winch: Some((winch_rx.as_raw_fd(), terminal)),
                },
                &mut host,
            );
        });
        assert!(matches!(
            exec_proto::Message::read_from(&mut guest).unwrap(),
            exec_proto::Message::StdinEof
        ));
        assert!(
            !worker.is_finished(),
            "the thread must keep watching the terminal"
        );
        drop(wake_tx);
        worker.join().unwrap();
        drop(pty);
    }

    #[test]
    fn a_tty_session_without_a_local_terminal_skips_raw_mode() {
        let (pipe_rx, _pipe_tx) = nix::unistd::pipe().unwrap();
        let fd = std::os::fd::AsFd::as_fd(&pipe_rx);
        assert!(setup_raw_terminal(fd).unwrap().is_none());
        assert_eq!(terminal_size(pipe_rx.as_raw_fd()), None);
    }
}
