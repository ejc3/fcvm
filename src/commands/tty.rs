//! Unified TTY handling for fcvm host side.
//!
//! This module provides a single implementation for running TTY sessions,
//! used by both `podman run -it` and `exec -it` paths.

use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use tracing::{debug, info, warn};

/// Global storage for terminal restoration on signal
/// Stores (stdin_fd, original_termios) when raw mode is active
static ORIG_TERMIOS: Mutex<Option<(i32, libc::termios)>> = Mutex::new(None);

/// Global flag to track if signal handlers are installed
static SIGNAL_HANDLERS_INSTALLED: AtomicBool = AtomicBool::new(false);

/// Install signal handlers for terminal restoration
fn install_signal_handlers() {
    if SIGNAL_HANDLERS_INSTALLED.swap(true, Ordering::SeqCst) {
        return; // Already installed
    }

    // Handler that restores terminal and re-raises the signal
    extern "C" fn signal_handler(sig: libc::c_int) {
        // Restore terminal if we have saved state
        if let Ok(guard) = ORIG_TERMIOS.lock() {
            if let Some((fd, termios)) = *guard {
                unsafe {
                    libc::tcsetattr(fd, libc::TCSANOW, &termios);
                }
            }
        }
        // Re-raise the signal with default handler
        unsafe {
            libc::signal(sig, libc::SIG_DFL);
            libc::raise(sig);
        }
    }

    unsafe {
        // Install handlers for common termination signals
        libc::signal(libc::SIGTERM, signal_handler as libc::sighandler_t);
        libc::signal(libc::SIGQUIT, signal_handler as libc::sighandler_t);
        libc::signal(libc::SIGHUP, signal_handler as libc::sighandler_t);
        // Note: SIGINT (Ctrl+C) is passed through to guest in raw mode
    }
}

/// Run a TTY session by listening on a Unix socket.
///
/// Used by `podman run -it` where the host listens and guest connects.
///
/// 1. Binds and accepts connection on Unix socket
/// 2. Delegates to `run_tty_session_connected()` for I/O handling
/// 3. Cleans up socket on exit
pub fn run_tty_session(socket_path: &str, tty: bool, interactive: bool) -> Result<i32> {
    // Remove stale socket if it exists
    let _ = std::fs::remove_file(socket_path);

    let listener =
        UnixListener::bind(socket_path).with_context(|| format!("binding to {}", socket_path))?;

    info!(socket = %socket_path, tty, interactive, "TTY session started");

    // Accept connection from guest (blocking)
    listener
        .set_nonblocking(false)
        .context("setting listener to blocking")?;
    let (stream, _) = listener.accept().context("accepting connection")?;

    debug!("TTY connection established");

    // Run the session with the connected stream
    let result = run_tty_session_connected(stream, tty, interactive);

    // Clean up socket
    let _ = std::fs::remove_file(socket_path);

    result
}

/// Run a TTY session with a pre-connected stream.
///
/// Used by `exec -it` where the host has already connected to the guest.
///
/// 1. Sets terminal to raw mode if `tty=true`
/// 2. Spawns reader thread (socket -> stdout)
/// 3. Spawns writer thread if `interactive=true` (stdin -> socket)
/// 4. Returns exit code from remote command
pub fn run_tty_session_connected(stream: UnixStream, tty: bool, interactive: bool) -> Result<i32> {
    // Set up raw terminal mode if TTY requested
    let stdin_fd = std::io::stdin().as_raw_fd();

    // Debug: check if stdin is non-blocking
    let stdin_flags = unsafe { libc::fcntl(stdin_fd, libc::F_GETFL) };
    let is_nonblocking = (stdin_flags & libc::O_NONBLOCK) != 0;
    debug!(
        "run_tty_session_connected: stdin_fd={}, flags=0x{:x}, O_NONBLOCK={}",
        stdin_fd, stdin_flags, is_nonblocking
    );
    let orig_termios = if tty {
        setup_raw_terminal(stdin_fd)?
    } else {
        None
    };

    // Flag to track completion
    let done = Arc::new(AtomicBool::new(false));

    // Clone stream for reader/writer
    let read_stream = stream.try_clone().context("cloning stream for reader")?;
    let mut write_stream = stream;

    // Spawn reader thread: socket -> stdout
    let reader_done = done.clone();
    let reader_thread = std::thread::spawn(move || reader_loop(read_stream, reader_done));

    // Spawn writer thread if interactive: stdin -> socket
    let writer_thread = if interactive {
        let writer_done = done.clone();
        Some(std::thread::spawn(move || {
            writer_loop(&mut write_stream, writer_done);
        }))
    } else {
        drop(write_stream);
        None
    };

    // Wait for reader to finish and get exit code
    let exit_code = reader_thread.join().ok().flatten().unwrap_or(0);

    // Signal writer to stop
    done.store(true, Ordering::Relaxed);

    // Restore terminal if we set raw mode
    if let Some(termios) = orig_termios {
        restore_terminal(stdin_fd, termios);
    }

    // Join writer thread to avoid zombie threads
    if let Some(handle) = writer_thread {
        let _ = handle.join();
    }

    Ok(exit_code)
}

/// Set up raw terminal mode
fn setup_raw_terminal(stdin_fd: i32) -> Result<Option<libc::termios>> {
    let is_tty = unsafe { libc::isatty(stdin_fd) == 1 };
    if !is_tty {
        bail!("TTY mode requires a terminal. Use without -t for non-interactive mode.");
    }

    // Install signal handlers for terminal restoration before modifying terminal
    install_signal_handlers();

    // Ensure stdin is in blocking mode (tokio may have set it non-blocking)
    unsafe {
        let flags = libc::fcntl(stdin_fd, libc::F_GETFL);
        if flags != -1 && (flags & libc::O_NONBLOCK) != 0 {
            libc::fcntl(stdin_fd, libc::F_SETFL, flags & !libc::O_NONBLOCK);
            debug!("setup_raw_terminal: cleared O_NONBLOCK from stdin");
        }
    }

    // Save original terminal settings
    let mut termios: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(stdin_fd, &mut termios) } != 0 {
        bail!("Failed to get terminal attributes");
    }
    let orig = termios;

    // Store in global for signal handler access
    if let Ok(mut guard) = ORIG_TERMIOS.lock() {
        *guard = Some((stdin_fd, orig));
    }

    // Set raw mode
    unsafe {
        libc::cfmakeraw(&mut termios);
    }
    if unsafe { libc::tcsetattr(stdin_fd, libc::TCSANOW, &termios) } != 0 {
        // Clear global on failure
        if let Ok(mut guard) = ORIG_TERMIOS.lock() {
            *guard = None;
        }
        bail!("Failed to set raw terminal mode");
    }

    Ok(Some(orig))
}

/// Restore terminal to original settings
fn restore_terminal(stdin_fd: i32, termios: libc::termios) {
    // Clear global first (signal handler won't need to restore anymore)
    if let Ok(mut guard) = ORIG_TERMIOS.lock() {
        *guard = None;
    }

    let ret = unsafe { libc::tcsetattr(stdin_fd, libc::TCSANOW, &termios) };
    if ret != 0 {
        warn!(
            "Failed to restore terminal settings: {}",
            std::io::Error::last_os_error()
        );
    }
}

/// Reader loop: read framed messages from socket, write to stdout
fn reader_loop(mut stream: std::os::unix::net::UnixStream, done: Arc<AtomicBool>) -> Option<i32> {
    let mut stdout = std::io::stdout().lock();
    let mut total_read: usize = 0;

    loop {
        if done.load(Ordering::Relaxed) {
            debug!("reader_loop: done flag set, exiting");
            break;
        }

        match exec_proto::Message::read_from(&mut stream) {
            Ok(exec_proto::Message::Data(data)) => {
                total_read += data.len();
                debug!(
                    "reader_loop: received {} bytes (total {})",
                    data.len(),
                    total_read
                );
                let _ = stdout.write_all(&data);
                let _ = stdout.flush();
            }
            Ok(exec_proto::Message::Exit(code)) => {
                debug!("reader_loop: received Exit({})", code);
                done.store(true, Ordering::Relaxed);
                return Some(code);
            }
            Ok(exec_proto::Message::Error(msg)) => {
                debug!("reader_loop: received Error({})", msg);
                let _ = writeln!(stdout, "\r\nError: {}\r", msg);
                let _ = stdout.flush();
                done.store(true, Ordering::Relaxed);
                return Some(1);
            }
            Ok(exec_proto::Message::Stdin(_)) => {
                // Should not receive STDIN from guest, ignore
            }
            Err(e) => {
                if e.kind() == std::io::ErrorKind::UnexpectedEof {
                    debug!("reader_loop: EOF");
                    // EOF without Exit message - return error
                    done.store(true, Ordering::Relaxed);
                    return Some(1);
                }
                debug!("reader_loop: error: {}", e);
                eprintln!("\r\nProtocol error: {}\r", e);
                done.store(true, Ordering::Relaxed);
                return Some(1);
            }
        }
    }
    // Should not reach here normally - return error if we do
    Some(1)
}

/// Writer loop: read from stdin, send STDIN messages to socket
fn writer_loop(stream: &mut std::os::unix::net::UnixStream, done: Arc<AtomicBool>) {
    let stdin_fd = std::io::stdin().as_raw_fd();
    let mut buf = [0u8; 1024];
    let mut total_written = 0usize;

    debug!(
        "writer_loop: starting, stdin_fd={}, waiting for stdin data",
        stdin_fd
    );

    // Use poll-based loop to avoid blocking forever and see when data arrives
    let mut poll_count = 0;
    let mut last_log_time = std::time::Instant::now();

    loop {
        if done.load(Ordering::Relaxed) {
            debug!("writer_loop: done flag set, exiting");
            break;
        }

        // Poll for 1 second
        let mut pollfd = libc::pollfd {
            fd: stdin_fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let poll_result = unsafe { libc::poll(&mut pollfd, 1, 1000) };
        poll_count += 1;

        // Log every 5 seconds
        if last_log_time.elapsed() > std::time::Duration::from_secs(5) {
            debug!(
                "writer_loop: poll #{}, result={}, revents=0x{:x}",
                poll_count, poll_result, pollfd.revents
            );
            last_log_time = std::time::Instant::now();
        }

        if poll_result < 0 {
            let errno = std::io::Error::last_os_error();
            debug!("writer_loop: poll error: {}", errno);
            break;
        } else if poll_result == 0 {
            // Timeout, no data yet
            continue;
        }

        // Data available! Read it
        debug!(
            "writer_loop: DATA AVAILABLE! poll_result={}, revents=0x{:x}",
            poll_result, pollfd.revents
        );

        let n = unsafe { libc::read(stdin_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };

        if n < 0 {
            let errno = std::io::Error::last_os_error();
            debug!("writer_loop: stdin read error: {}", errno);
            break;
        } else if n == 0 {
            debug!("writer_loop: EOF on stdin");
            break;
        } else {
            let n = n as usize;
            total_written += n;
            debug!(
                "writer_loop: read {} bytes from stdin (total {})",
                n, total_written
            );
            if exec_proto::write_stdin(stream, &buf[..n]).is_err() {
                debug!("writer_loop: write_stdin failed");
                break;
            }
        }
    }
    debug!(
        "writer_loop: exiting, total written: {} bytes",
        total_written
    );
}
