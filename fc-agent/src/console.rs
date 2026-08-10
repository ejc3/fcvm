//! Wedge-proof serial console plumbing + snapshot quiesce support.
//!
//! fc-agent's fd 1/2 used to be dup2'd straight onto `/dev/console`. That made every
//! `eprintln!` a BLOCKING write into the guest UART: after a snapshot captured the
//! 8250 mid-transmit, the restored device never delivers the TX interrupt the driver
//! waits for, the tty's 4096-byte output buffer fills, and the next `eprintln!`
//! blocks its thread forever — including inside async tasks (the exec-server accept
//! loop logged before spawning, so health checks wedged after ~4KB of output).
//!
//! New model: fd 1/2 are dup2'd onto the write end of a pipe. A dedicated writer
//! thread is the ONLY writer to `/dev/console`:
//!
//! ```text
//! eprintln!/children ──> pipe ──> writer thread ──> /dev/console (O_NONBLOCK)
//!                                   │ bounded pending buffer (drop + count on overflow)
//!                                   └ gate: console writes suspended during snapshot quiesce
//! ```
//!
//! A dead console degrades to LOST LOG LINES (counted, summarized on recovery),
//! never a blocked thread: the writer always drains the pipe, so producers block at
//! most momentarily on a transiently full pipe, and console writes are non-blocking.
//!
//! Snapshot quiesce (`quiesce_for_snapshot`) gives the pre-start snapshot handshake
//! a STRUCTURAL guarantee that no byte is in UART TX when the host pauses the VM:
//! 1. politeness flush — wait until everything already logged has left the pipe and
//!    been written to the console (`pipe empty && enqueued == disposed`);
//! 2. gate — the writer thread stops writing to the console (bytes accumulate in its
//!    pending buffer instead; `eprintln!` keeps working and never blocks);
//! 3. drain — poll `TIOCOUTQ` on the console fd until 0. If it never reaches 0
//!    within the bounded window (or its state is unknowable), quiesce releases the
//!    gate and FAILS: the caller must abort the handshake — no `cache-ready`, no
//!    snapshot — rather than let the host pause a VM whose UART may be mid-transmit.
//!
//! Why `TIOCOUTQ == 0` proves the UART is idle: the 8250 driver drains its circular
//! buffer to THR only inside its IRQ handler under the port lock, and the same
//! critical section disables THRI (`__stop_tx`) when the buffer empties. Firecracker
//! consumes each THR write synchronously on the vCPU MMIO exit. `TIOCOUTQ` takes the
//! same port lock, so reading 0 happens-after the final THR write AND the THRI
//! disable — no bytes in flight, no armed TX interrupt to capture in the snapshot.
//! The gate guarantees the UART STAYS idle until the guard is dropped, no matter
//! which other task logs concurrently.
//!
//! Scope: this covers fc-agent's OWN console writes — the only userspace console
//! writer at the cache-ready point in this rootfs (no getty on ttyS0; journald
//! console forwarding is not enabled; systemd is idle between boot and the
//! container start that follows the ack). Kernel printk is pause-safe by
//! construction (polled THRE path, no THRI). A future rootfs change that adds
//! another userspace console writer would reopen the mid-TX window for ITS
//! writes; the host-side dead-serial watchdog (src/commands/snapshot.rs) exists
//! to catch exactly that regression loudly.

use std::collections::VecDeque;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::fcntl::{fcntl, FcntlArg, OFlag};
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::unistd::{read, write};

/// Upper bound on bytes buffered while the console is gated or stalled.
/// Beyond this, new bytes are dropped (and counted; one summary line is
/// emitted when the console recovers). Sized for the WORST-CASE gate hold:
/// the quiesce gate is held for the entire cache-ready→ack handshake, which
/// host `cache-wait` keepalives can extend to the absolute 600s cap
/// (`notify_cache_ready_and_wait` in container.rs) — not just a short
/// pre-pause window. 1 MiB absorbs ~1.7 KiB/s of gated log traffic for that
/// full window, well above fc-agent's log rate while idling in the handshake
/// (a few short lines per poll cycle). Dropping on overflow stays deliberate:
/// bounded memory against a wedged console beats unbounded growth, and drops
/// are counted and summarized rather than silent.
const PENDING_CAP: usize = 1024 * 1024;

/// Writer thread poll tick. Bounds how quickly the writer notices a gate
/// transition; pipe data wakes it immediately via POLLIN.
const POLL_TICK_MS: u16 = 100;

/// Bound on each quiesce phase (politeness flush / UART drain). The console
/// works pre-snapshot, so these normally complete in microseconds; the cap
/// only prevents an unexpected wedge from stalling container startup.
const QUIESCE_PHASE_TIMEOUT: Duration = Duration::from_secs(5);

static CONSOLE: OnceLock<Arc<ConsoleShared>> = OnceLock::new();

/// Shared state between the writer thread and quiesce callers.
pub struct ConsoleShared {
    console_fd: OwnedFd,
    pipe_rd: OwnedFd,
    /// Kept so the pipe outlives any exotic fd 1/2 juggling; fd 1/2 are dups.
    pipe_wr: OwnedFd,
    /// Bytes the writer thread has taken out of the pipe (responsibility taken).
    enqueued: AtomicU64,
    /// Bytes written to the console or deliberately dropped.
    /// Invariant: `enqueued - disposed == pending buffer length`.
    disposed: AtomicU64,
    /// Bytes dropped since the last recovery summary.
    dropped: AtomicU64,
    /// While true, the writer thread must not write to the console.
    gate: AtomicBool,
    /// Held around every console `write()`; `quiesce` locks it once after
    /// setting `gate` so no console write is in flight or can start afterwards.
    write_lock: Mutex<()>,
}

/// RAII guard for the console gate. Dropping it re-enables console writes
/// (on every return path of the cache-ready handshake, including the restored
/// VM resuming inside it).
pub struct QuiesceGuard {
    state: Option<Arc<ConsoleShared>>,
}

impl Drop for QuiesceGuard {
    fn drop(&mut self) {
        if let Some(state) = self.state.take() {
            state.gate.store(false, Ordering::SeqCst);
            // Writer notices within POLL_TICK_MS and flushes its pending buffer.
        }
    }
}

/// The UART could not be proven idle within the drain window. The gate has
/// already been released — the caller MUST abort the cache-ready handshake
/// (send nothing, take no snapshot): pausing the VM in this state can capture
/// the 8250 mid-transmit and poison the snapshot for every restore.
#[derive(Debug)]
pub struct QuiesceError;

impl std::fmt::Display for QuiesceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "console UART did not drain within the quiesce window")
    }
}

impl std::error::Error for QuiesceError {}

/// Redirect fd 1/2 to the console pipe and start the writer thread.
///
/// Replaces the old direct dup2-to-/dev/console: systemd's `journal+console`
/// path dies when journald crashes after a snapshot restore (journal corrupted
/// mid-write), so fc-agent owns its console output directly — but through the
/// pipe + writer thread so a dead console can never block a logging thread.
///
/// If `/dev/console` cannot be opened, fd 1/2 are left untouched.
pub fn init() {
    let console = match std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK)
        .open("/dev/console")
    {
        Ok(f) => OwnedFd::from(f),
        Err(_) => return,
    };

    let state = match setup(console) {
        Ok(s) => s,
        Err(_) => return,
    };

    // All existing eprintln!/println! calls (and child processes inheriting
    // fd 1/2) now feed the pipe. dup2 clears CLOEXEC on fds 1/2. On dup2
    // failure the fds keep their previous target (direct console) — degraded
    // but functional, and there is nowhere useful left to report it.
    unsafe {
        let r1 = libc::dup2(state.pipe_wr.as_raw_fd(), 1);
        let r2 = libc::dup2(state.pipe_wr.as_raw_fd(), 2);
        if r1 < 0 || r2 < 0 {
            return;
        }
    }

    let _ = CONSOLE.set(state);
}

/// Build the pipe + writer thread around an already-open console fd.
/// Separated from [`init`] so tests can use a fake console without
/// re-plumbing the test harness's own fd 1/2.
fn setup(console_fd: OwnedFd) -> nix::Result<Arc<ConsoleShared>> {
    // Write end stays BLOCKING: eprintln! panics on write errors, and a
    // transiently full pipe must block the producer momentarily (the writer
    // thread always drains it), never return EAGAIN into eprintln!.
    //
    // nix 0.29 API split: `fcntl` and `read` still take `RawFd`, while `write`
    // and `poll` already take `AsFd` — the `as_raw_fd()` calls here and at the
    // writer-loop `read` are required by those signatures, not an opt-out from
    // the safe wrappers. (Raw `libc::ioctl` below is different: nix has no
    // TIOCOUTQ/FIONREAD wrapper.)
    let (pipe_rd, pipe_wr) = nix::unistd::pipe2(OFlag::O_CLOEXEC)?;
    let flags = fcntl(pipe_rd.as_raw_fd(), FcntlArg::F_GETFL)?;
    fcntl(
        pipe_rd.as_raw_fd(),
        FcntlArg::F_SETFL(OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK),
    )?;
    // Belt-and-braces: the console fd must be non-blocking even if the caller
    // forgot O_NONBLOCK at open time.
    let cflags = fcntl(console_fd.as_raw_fd(), FcntlArg::F_GETFL)?;
    fcntl(
        console_fd.as_raw_fd(),
        FcntlArg::F_SETFL(OFlag::from_bits_truncate(cflags) | OFlag::O_NONBLOCK),
    )?;

    let state = Arc::new(ConsoleShared {
        console_fd,
        pipe_rd,
        pipe_wr,
        enqueued: AtomicU64::new(0),
        disposed: AtomicU64::new(0),
        dropped: AtomicU64::new(0),
        gate: AtomicBool::new(false),
        write_lock: Mutex::new(()),
    });

    let thread_state = state.clone();
    std::thread::Builder::new()
        .name("console-writer".into())
        .spawn(move || writer_loop(thread_state))
        .map_err(|_| Errno::EAGAIN)?;

    Ok(state)
}

/// The dedicated console writer thread. Sole writer to the console fd.
/// Panic-free by construction: every syscall result is handled.
fn writer_loop(state: Arc<ConsoleShared>) {
    let mut pending: VecDeque<u8> = VecDeque::new();
    let mut buf = [0u8; 8192];

    loop {
        // Poll: always watch the pipe; watch the console for writability only
        // when there is something to flush and the gate is open.
        let want_write = !pending.is_empty() && !state.gate.load(Ordering::SeqCst);
        {
            let mut fds = [
                PollFd::new(state.pipe_rd.as_fd(), PollFlags::POLLIN),
                PollFd::new(state.console_fd.as_fd(), PollFlags::POLLOUT),
            ];
            let nfds = if want_write { 2 } else { 1 };
            match poll(&mut fds[..nfds], PollTimeout::from(POLL_TICK_MS)) {
                Ok(_) => {}
                Err(Errno::EINTR) => continue,
                Err(_) => {
                    // Poll on our own fds failing is unrecoverable; back off so a
                    // persistent error can't spin the CPU. Producers still block on
                    // the pipe at worst — same as a full pipe.
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
            }
        }

        // 1) Drain the pipe (non-blocking) so producers can never wedge on it.
        loop {
            match read(state.pipe_rd.as_raw_fd(), &mut buf) {
                Ok(0) => break, // all write ends closed (cannot happen: we hold one)
                Ok(n) => {
                    state.enqueued.fetch_add(n as u64, Ordering::SeqCst);
                    let room = PENDING_CAP.saturating_sub(pending.len());
                    let keep = n.min(room);
                    pending.extend(&buf[..keep]);
                    if keep < n {
                        let over = (n - keep) as u64;
                        // Dropped bytes are disposed: the enqueued/disposed
                        // invariant tracks the pending buffer, not the console.
                        state.dropped.fetch_add(over, Ordering::SeqCst);
                        state.disposed.fetch_add(over, Ordering::SeqCst);
                    }
                    if n < buf.len() {
                        break;
                    }
                }
                Err(Errno::EAGAIN) => break,
                Err(Errno::EINTR) => continue,
                Err(_) => break,
            }
        }

        // 2) Flush pending to the console — never while gated, never blocking.
        let mut wrote_any = false;
        if !pending.is_empty() {
            let _guard = state.write_lock.lock().unwrap_or_else(|e| e.into_inner());
            // Checked under the lock: after quiesce() sets the gate and cycles
            // this lock, no write can start here.
            if !state.gate.load(Ordering::SeqCst) {
                while !pending.is_empty() {
                    let (head, _) = pending.as_slices();
                    match write(state.console_fd.as_fd(), head) {
                        Ok(0) => break,
                        Ok(n) => {
                            pending.drain(..n);
                            state.disposed.fetch_add(n as u64, Ordering::SeqCst);
                            wrote_any = true;
                        }
                        Err(Errno::EAGAIN) => break, // console TX full — try next tick
                        Err(Errno::EINTR) => continue,
                        Err(_) => {
                            // Hard console error (EIO, ENODEV, ...): drop what we
                            // hold, count it, and keep running — the console may
                            // come back, and producers must never feel this.
                            let len = pending.len() as u64;
                            pending.clear();
                            state.dropped.fetch_add(len, Ordering::SeqCst);
                            state.disposed.fetch_add(len, Ordering::SeqCst);
                            break;
                        }
                    }
                }
            }
        }

        // 3) Console proven alive again after drops: emit ONE summary line.
        // Only after a fully successful flush (wrote_any && empty) so a dead
        // console can't loop summary → drop → summary.
        if wrote_any && pending.is_empty() {
            let dropped = state.dropped.swap(0, Ordering::SeqCst);
            if dropped > 0 {
                let line = format!(
                    "[fc-agent] console: dropped {} bytes of log output while the console was stalled\n",
                    dropped
                );
                // Goes through the normal pending path (counted as enqueued so
                // the enqueued/disposed invariant holds).
                state
                    .enqueued
                    .fetch_add(line.len() as u64, Ordering::SeqCst);
                pending.extend(line.as_bytes());
            }
        }
    }
}

impl ConsoleShared {
    /// Bytes currently readable from the pipe (written by producers, not yet
    /// picked up by the writer thread).
    fn pipe_backlog(&self) -> i64 {
        let mut n: libc::c_int = 0;
        let rc = unsafe { libc::ioctl(self.pipe_rd.as_raw_fd(), libc::FIONREAD, &mut n) };
        if rc == 0 {
            n as i64
        } else {
            -1
        }
    }

    /// Bytes still queued in the tty/UART output buffer.
    ///
    /// Returns -1 when the state is UNKNOWN (unexpected `TIOCOUTQ` failure).
    /// Callers must treat unknown as "not drained": converting "cannot verify
    /// the UART" into "the UART is idle" would let quiesce vouch for a drain
    /// it never observed.
    fn uart_backlog(&self) -> i64 {
        let mut n: libc::c_int = 0;
        let rc = unsafe { libc::ioctl(self.console_fd.as_raw_fd(), libc::TIOCOUTQ, &mut n) };
        if rc == 0 {
            n as i64
        } else if Errno::last() == Errno::ENOTTY {
            // Not a tty (tests use pipes/files): there is no 8250 UART, so
            // nothing can be queued in one.
            0
        } else {
            -1
        }
    }

    /// Everything logged before this call has been handed to the console (or
    /// dropped): the pipe is empty and the writer holds no pending bytes.
    fn flushed(&self) -> bool {
        // Order matters: pipe first. Best-effort: bytes the writer thread has
        // read from the pipe but not yet counted into `enqueued` are invisible
        // to both checks, so this can be momentarily true while data sits in
        // the writer's local buffer. That is fine for its only caller — the
        // politeness phase of quiesce (cosmetic); the safety guarantee comes
        // from the gate + TIOCOUTQ drain, not from this check.
        self.pipe_backlog() == 0
            && self.enqueued.load(Ordering::SeqCst) == self.disposed.load(Ordering::SeqCst)
    }

    /// See [`quiesce_for_snapshot`].
    fn quiesce(self: &Arc<Self>) -> Result<QuiesceGuard, QuiesceError> {
        self.quiesce_with_timeout(QUIESCE_PHASE_TIMEOUT)
    }

    /// [`Self::quiesce`] with an explicit per-phase bound. Tests shrink it so
    /// the no-drain abort path doesn't cost real seconds.
    fn quiesce_with_timeout(
        self: &Arc<Self>,
        phase_timeout: Duration,
    ) -> Result<QuiesceGuard, QuiesceError> {
        // Phase 1 (politeness): let already-logged lines reach the console so
        // the quiesce announcement lands before the host pauses the VM. Purely
        // cosmetic for safety — phases 2+3 provide the guarantee.
        let deadline = Instant::now() + phase_timeout;
        while !self.flushed() {
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }

        // Phase 2 (gate): stop console writes. Setting the flag and then
        // cycling the write lock waits out any in-flight console write; every
        // later write attempt re-checks the flag under the same lock. From
        // here, ZERO new bytes can enter the UART regardless of which task
        // logs concurrently — logs accumulate in the writer's pending buffer.
        self.gate.store(true, Ordering::SeqCst);
        drop(self.write_lock.lock().unwrap_or_else(|e| e.into_inner()));
        let guard = QuiesceGuard {
            state: Some(self.clone()),
        };

        // Phase 3 (drain): wait for the UART to go fully idle (TIOCOUTQ == 0
        // also proves THRI is disarmed — see module docs). Monotone: the gate
        // stops all refills. Terminates because the console works pre-snapshot;
        // the cap only prevents an unexpected wedge from stalling startup.
        let deadline = Instant::now() + phase_timeout;
        let mut drained = false;
        while Instant::now() < deadline {
            if self.uart_backlog() == 0 {
                drained = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        if !drained {
            // The UART cannot be proven idle: a snapshot taken now could
            // capture the 8250 mid-transmit and every restore of it would have
            // a dead serial console. Release the gate (dropping the guard —
            // the VM is staying live and cold, so logging must keep flowing)
            // and surface the failure so the caller aborts the handshake
            // instead of inviting the host to pause us.
            drop(guard);
            eprintln!(
                "[fc-agent] WARNING: console did not drain before snapshot quiesce; \
                 aborting the cache-ready handshake so no snapshot can capture the \
                 UART mid-transmit"
            );
            return Err(QuiesceError);
        }

        Ok(guard)
    }
}

/// Make the serial console provably quiet for a host-side VM pause.
///
/// Called before sending `cache-ready` (which triggers the host's pre-start
/// snapshot pause). On success, until the returned guard drops, no byte
/// reaches the UART: the snapshot cannot capture the 8250 mid-transmit, so
/// its restores keep a working serial console. `eprintln!` stays usable
/// throughout — lines are held in the writer's pending buffer and flushed
/// when the guard drops (ack received, restore detected, or failure).
///
/// `Err` means the UART could not be proven idle: the gate is NOT held, and
/// the caller must NOT send `cache-ready` — the only safe outcome is to
/// continue cold with no snapshot artifact.
///
/// No-op `Ok` guard if [`init`] didn't run (no /dev/console).
pub fn quiesce_for_snapshot() -> Result<QuiesceGuard, QuiesceError> {
    match CONSOLE.get() {
        Some(state) => state.quiesce(),
        None => Ok(QuiesceGuard { state: None }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::fd::AsRawFd;

    /// Fake console: a pipe whose read end the test controls. Not reading it
    /// (after filling the buffer) simulates a stalled/dead console.
    fn fake_console() -> (OwnedFd, std::fs::File) {
        let (rd, wr) = nix::unistd::pipe2(OFlag::O_CLOEXEC).unwrap();
        (wr, std::fs::File::from(rd))
    }

    fn write_pipe(state: &Arc<ConsoleShared>, data: &[u8]) {
        let mut left = data;
        while !left.is_empty() {
            match write(state.pipe_wr.as_fd(), left) {
                Ok(n) => left = &left[n..],
                Err(Errno::EINTR) => continue,
                Err(e) => panic!("pipe write failed: {}", e),
            }
        }
    }

    fn wait_until(mut cond: impl FnMut() -> bool, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        cond()
    }

    /// Append whatever the fake console currently holds to `sink` WITHOUT
    /// blocking (FIONREAD probe, read only what is there). Blocking reads are
    /// forbidden in these tests: `flushed()` can be transiently true before
    /// bytes reach the console, and once the gate closes a blocking read can
    /// only return after the guard drops — which the test does after the read.
    /// A lost race must fail an assert, never hang the suite.
    fn drain_console(console_rd: &mut std::fs::File, sink: &mut Vec<u8>) {
        let mut out = [0u8; 65536];
        loop {
            let mut avail: libc::c_int = 0;
            let rc = unsafe { libc::ioctl(console_rd.as_raw_fd(), libc::FIONREAD, &mut avail) };
            if rc != 0 || avail == 0 {
                return;
            }
            match console_rd.read(&mut out) {
                Ok(0) => return,
                Ok(n) => sink.extend_from_slice(&out[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => panic!("fake console read failed: {}", e),
            }
        }
    }

    #[test]
    fn test_console_relay_and_flush() {
        let (console_wr, mut console_rd) = fake_console();
        let state = setup(console_wr).unwrap();
        let mut seen: Vec<u8> = Vec::new();

        write_pipe(&state, b"hello console\n");
        assert!(
            wait_until(
                || {
                    drain_console(&mut console_rd, &mut seen);
                    seen.as_slice() == b"hello console\n"
                },
                Duration::from_secs(5)
            ),
            "writer must relay all bytes to the console, got {:?}",
            String::from_utf8_lossy(&seen)
        );
        assert!(
            wait_until(|| state.flushed(), Duration::from_secs(5)),
            "writer must account every relayed byte as disposed"
        );
    }

    #[test]
    fn test_console_gate_holds_bytes_then_releases() {
        let (console_wr, mut console_rd) = fake_console();
        let state = setup(console_wr).unwrap();
        let mut seen: Vec<u8> = Vec::new();

        // Wait for the bytes to actually REACH the console, not merely for
        // `flushed()`: that check can be transiently true while the bytes sit
        // in the writer thread's local buffer, and gating in that window would
        // hold "before-gate" back until the guard drops.
        write_pipe(&state, b"before-gate\n");
        assert!(
            wait_until(
                || {
                    drain_console(&mut console_rd, &mut seen);
                    seen.as_slice() == b"before-gate\n"
                },
                Duration::from_secs(5)
            ),
            "pre-gate bytes must reach the console, got {:?}",
            String::from_utf8_lossy(&seen)
        );

        let guard = state
            .quiesce()
            .expect("quiesce on a pipe-backed console must report drained");

        // Logged while gated: consumed from the pipe but NOT written to the console.
        write_pipe(&state, b"held-line\n");
        assert!(
            wait_until(
                || state.pipe_backlog() == 0
                    && state.enqueued.load(Ordering::SeqCst)
                        > state.disposed.load(Ordering::SeqCst),
                Duration::from_secs(5)
            ),
            "gated bytes must move to the pending buffer, not the console"
        );

        // Nothing new may arrive on the fake console while gated: the held
        // bytes are accounted in the pending buffer and the gate was closed
        // (write lock cycled) before they were written.
        drain_console(&mut console_rd, &mut seen);
        assert_eq!(
            seen.as_slice(),
            b"before-gate\n",
            "console must not receive bytes while gated"
        );

        drop(guard);

        assert!(
            wait_until(
                || {
                    drain_console(&mut console_rd, &mut seen);
                    seen.as_slice() == b"before-gate\nheld-line\n"
                },
                Duration::from_secs(5)
            ),
            "dropping the guard must flush held bytes, got {:?}",
            String::from_utf8_lossy(&seen)
        );
    }

    #[test]
    fn test_console_quiesce_aborts_when_uart_never_drains() {
        use nix::sys::socket::{socketpair, AddressFamily, SockFlag, SockType};

        // A socketpair "console": on sockets, `TIOCOUTQ` (== `SIOCOUTQ`)
        // genuinely reports bytes the peer has not consumed — a never-read
        // peer models a UART whose TX queue never drains.
        let (console_fd, peer) = socketpair(
            AddressFamily::Unix,
            SockType::Stream,
            None,
            SockFlag::SOCK_CLOEXEC,
        )
        .unwrap();
        let state = setup(console_fd).unwrap();

        write_pipe(&state, b"stuck-in-uart\n");
        assert!(
            wait_until(|| state.uart_backlog() > 0, Duration::from_secs(5)),
            "socket console must report an undrained TX backlog"
        );

        let result = state.quiesce_with_timeout(Duration::from_millis(200));
        assert!(
            result.is_err(),
            "quiesce must ABORT when the UART cannot be proven idle — returning a \
             guard here would let the host snapshot the VM mid-transmit"
        );
        // The failed quiesce must not leave the console gated: the handshake
        // is aborted, the VM continues cold, and logging must keep flowing.
        assert!(
            !state.gate.load(Ordering::SeqCst),
            "failed quiesce must release the gate before returning"
        );

        drop(peer);
    }

    #[test]
    fn test_console_stall_drops_and_summarizes_on_recovery() {
        let (console_wr, mut console_rd) = fake_console();
        // Shrink the fake console's buffer so a stall is cheap to produce.
        unsafe {
            libc::fcntl(console_wr.as_raw_fd(), libc::F_SETPIPE_SZ, 4096);
        }
        let state = setup(console_wr).unwrap();

        // Stall the console (nobody reads it) and overwhelm pending: the fake
        // console absorbs ~4KB, the pending buffer holds PENDING_CAP bytes,
        // the rest must be DROPPED — and no producer may ever block.
        let chunk = vec![b'x'; 8192];
        let total = 4096 + PENDING_CAP + 64 * 1024;
        let mut written = 0;
        while written < total {
            write_pipe(&state, &chunk);
            written += chunk.len();
        }
        assert!(
            wait_until(
                || state.dropped.load(Ordering::SeqCst) > 0,
                Duration::from_secs(10)
            ),
            "overflow beyond the pending cap must be dropped, not block"
        );

        // Recover the console: drain everything. The writer must flush its
        // pending bytes and append ONE summary line about the dropped bytes.
        let mut sunk: Vec<u8> = Vec::new();
        assert!(
            wait_until(
                || {
                    // Observe the promised external effect itself. `dropped` is
                    // cleared immediately before the summary is appended to the
                    // writer's pending queue, so `flushed() && dropped == 0` has
                    // a legitimate transient window before the summary is
                    // visible on the console.
                    drain_console(&mut console_rd, &mut sunk);
                    String::from_utf8_lossy(&sunk)
                        .contains("bytes of log output while the console was stalled")
                },
                Duration::from_secs(10)
            ),
            "writer must emit the recovery summary after the console drains"
        );

        let text = String::from_utf8_lossy(&sunk);
        let summaries = text.matches("dropped").count();
        assert_eq!(
            summaries,
            1,
            "exactly one recovery summary line, got {}: {}",
            summaries,
            text.chars().rev().take(200).collect::<String>()
        );
        assert!(text.contains("bytes of log output while the console was stalled"));
    }

    #[test]
    fn test_console_quiesce_noop_without_init() {
        // Must not panic or block when /dev/console was never opened.
        let guard = QuiesceGuard { state: None };
        drop(guard);
    }
}
