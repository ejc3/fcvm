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
//! A snapshot of a running VM (a startup snapshot, `fcvm snapshot create`) takes the
//! same hold through the boundary's one-shots: `--prepare-snapshot-network` asks the
//! agent over [`SOURCE_GATE_SOCKET`] to quiesce and keep the hold, and
//! `--resume-snapshot-network` (on the source) or the restore watcher (on a clone,
//! which inherits the hold in the snapshot) releases it. The gate is a count, so this
//! hold and the cache handshake's can overlap.
//!
//! Restore log: every byte the writer puts on the 8250 is an MMIO write the VMM handles
//! in userspace, and a restore's progress lines were about 2.7 KB of them per clone. From
//! the snapshot boundary until [`RESTORE_LOG_WINDOW`] later, the writer keeps fc-agent's
//! progress lines ([`Route::Progress`]) off the console and hands them to the host over
//! the output vsock connection once the host has acknowledged the restore and the
//! connection is re-established ([`ProgressSink`]), each with the time it was logged. One
//! console line says so, which is also what the host's dead-serial check reads. The
//! console still gets, at once and in order, everything held so far plus the line itself
//! when a line is not a progress line (an error, a warning, a panic, a child's output),
//! and everything held when the window ends first: no acknowledgement or no connection
//! within the window, a refused hand-off, a new snapshot's quiesce, a shutdown. A held
//! line that goes to the console ends in ` held_ms=<n>`, how long ago it was logged. A guest
//! that stops running inside the window without printing any such line takes its held
//! progress lines with it; the host then has no acknowledgement for that restore.
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
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::fcntl::{fcntl, FcntlArg, OFlag};
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::unistd::{read, write};

/// Upper bound on bytes buffered while the console is gated or stalled.
/// Beyond this, new bytes are dropped (and counted; one summary line is
/// emitted when the console recovers). Sized for the WORST-CASE gate hold:
/// the quiesce gate is held from cache-ready until the snapshot boundary
/// severs the handshake session or the handshake returns, and host
/// `cache-wait` keepalives can extend that to the absolute 600s cap
/// (`run_cache_handshake` in container.rs) — not just a short
/// pre-pause window. 1 MiB absorbs ~1.7 KiB/s of gated log traffic for that
/// full window, well above fc-agent's log rate while idling in the handshake
/// (a few short lines per poll cycle). Dropping on overflow stays deliberate:
/// bounded memory against a wedged console beats unbounded growth, and drops
/// are counted and summarized rather than silent.
const PENDING_CAP: usize = 1024 * 1024;

/// How long after a snapshot boundary a restore's progress lines are kept off the console
/// (see the module doc). A restore that has not been acknowledged and reconnected by then
/// gets its lines on the console, so a restore that hangs shows where within this bound.
/// Resume to ready takes tens of milliseconds alone. In 128-way ramps to 1000 clones on
/// the File backend its 99th percentile was 4 to 8 s, and 2 of 6,000 clones took over 10 s.
/// The host waits 15 minutes for the acknowledgement.
const RESTORE_LOG_WINDOW: Duration = Duration::from_secs(10);

/// Upper bound on progress lines held for the host. Past it the window ends and they go to
/// the console, which has its own bound ([`PENDING_CAP`]).
const HELD_CAP: usize = 256 * 1024;

/// Where one line of the agent's output goes while a restore log window is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    /// fc-agent's own line on the success path: held, then sent to the host over the
    /// output connection.
    Progress,
    /// A `[fcvm-vitals]` sample: to the console now, and the window stays open. The
    /// samples exist for a guest that can no longer reach the host any other way.
    Sample,
    /// Everything else: to the console now, after every line held so far, and the window
    /// ends. fc-agent's errors and warnings, a `VITALS` block, and any line that is not
    /// fc-agent's own (a panic message, a child's output, a failpoint).
    Urgent,
}

/// What a line of fc-agent's that reports a failure starts with, after `[fc-agent] `. The
/// first four are the host's list (`console_line_is_an_agent_error` in src/utils.rs), which
/// logs those lines at WARN; `VITALS` opens the resource block printed at a failure site.
const URGENT_WORDS: [&str; 5] = ["FATAL", "ERROR", "Error: ", "WARNING", "VITALS"];

/// Route one line (with or without its newline). A line is a progress line only when it is
/// provably fc-agent's own and names no failure; anything unrecognised is urgent.
fn classify(line: &[u8]) -> Route {
    if line.starts_with(b"[fcvm-vitals]") {
        return Route::Sample;
    }
    let Some(message) = line.strip_prefix(b"[fc-agent] ") else {
        return Route::Urgent;
    };
    if URGENT_WORDS
        .iter()
        .any(|word| message.starts_with(word.as_bytes()))
    {
        Route::Urgent
    } else {
        Route::Progress
    }
}

/// One progress line held for the host.
pub struct ProgressLine {
    /// When the writer thread routed the line: as it took it out of the pipe, or, for a
    /// line that waited behind a snapshot's quiesce guard, when the window opened.
    pub logged: Instant,
    /// The line as it was written, without its newline.
    pub bytes: Vec<u8>,
}

/// A [`ProgressSink`] took none of the lines it was handed.
#[derive(Debug)]
pub struct Refused;

/// The connection that carries a restore's progress lines to the host.
pub trait ProgressSink: Send + Sync {
    /// Whether the connection is established now.
    fn connected(&self) -> bool;
    /// Hand `lines` over, in order: all of them or, refused, none.
    fn send(&self, lines: &[ProgressLine]) -> Result<(), Refused>;
}

/// What the callers ask of the writer thread about the restore log. Guarded by
/// [`ConsoleShared::write_lock`], which the writer holds while it routes and writes, so a
/// caller that sets the window and then drops a quiesce guard knows the writer routes the
/// bytes held behind that guard as the window's.
#[derive(Default)]
struct LogCtl {
    /// When the open window ends; None when none is open.
    window: Option<Instant>,
    /// Counts the windows opened, so the writer tells a new one from the one it knew.
    generation: u64,
    /// The host acknowledged the restore the open window belongs to.
    acknowledged: bool,
    /// None until the agent registers its output connection; without it no window opens.
    sink: Option<Arc<dyn ProgressSink>>,
}

/// The writer thread's side of the restore log.
#[derive(Default)]
struct RestoreLog {
    /// The generation of the window the fields below belong to; None outside a window.
    generation: Option<u64>,
    /// Bytes of the window not routed yet: a line whose newline has not arrived.
    inbox: Vec<u8>,
    /// Progress lines held for the host.
    held: Vec<ProgressLine>,
    held_bytes: usize,
    /// The first hand-off happened; later progress lines follow it.
    forwarding: bool,
}

/// Append console-bound bytes to the writer's backlog, dropping (and counting) what does
/// not fit under [`PENDING_CAP`].
fn push_console(state: &ConsoleShared, pending: &mut VecDeque<u8>, bytes: &[u8]) {
    let room = PENDING_CAP.saturating_sub(pending.len());
    let keep = bytes.len().min(room);
    pending.extend(&bytes[..keep]);
    if keep < bytes.len() {
        let over = (bytes.len() - keep) as u64;
        // Dropped bytes are disposed: the enqueued/disposed invariant tracks what the
        // writer still holds, not the console.
        state.dropped.fetch_add(over, Ordering::SeqCst);
        state.disposed.fetch_add(over, Ordering::SeqCst);
    }
}

impl RestoreLog {
    /// Route `fresh`, the bytes just read from the pipe. Called with `write_lock` held.
    fn route(
        &mut self,
        state: &ConsoleShared,
        ctl: &mut LogCtl,
        fresh: &[u8],
        pending: &mut VecDeque<u8>,
        cursor: &mut LineCursor,
        now: Instant,
    ) {
        if self.generation.is_some()
            && (ctl.window.is_none() || self.generation != Some(ctl.generation))
        {
            // A caller ended the window this thread was in.
            self.spill(state, pending, now);
        }
        let Some(deadline) = ctl.window else {
            push_console(state, pending, fresh);
            return;
        };
        if self.generation.is_none() {
            // The window opens. What the console has not taken yet (the lines held behind
            // a snapshot's quiesce guard) is routed with what follows.
            self.generation = Some(ctl.generation);
            self.inbox.extend(pending.drain(..));
            cursor.cleared();
        }
        self.inbox.extend_from_slice(fresh);
        if now >= deadline {
            return self.end(state, ctl, pending, now);
        }

        let mut routed = 0;
        while let Some(at) = self.inbox[routed..].iter().position(|&b| b == b'\n') {
            let start = routed;
            routed += at + 1;
            let line = &self.inbox[start..routed];
            match classify(line) {
                Route::Sample => push_console(state, pending, line),
                Route::Progress => {
                    self.held_bytes += line.len();
                    self.held.push(ProgressLine {
                        logged: now,
                        bytes: line[..line.len() - 1].to_vec(),
                    });
                }
                Route::Urgent => {
                    // The held lines first, then this one and whatever follows it.
                    let rest = self.inbox.split_off(start);
                    self.inbox.clear();
                    self.end(state, ctl, pending, now);
                    push_console(state, pending, &rest);
                    return;
                }
            }
        }
        self.inbox.drain(..routed);
        if self.held_bytes + self.inbox.len() > HELD_CAP {
            return self.end(state, ctl, pending, now);
        }

        if self.held.is_empty() || !(self.forwarding || ctl.acknowledged) {
            return;
        }
        let Some(sink) = ctl.sink.as_ref().filter(|sink| sink.connected()) else {
            return;
        };
        if sink.send(&self.held).is_err() {
            return self.end(state, ctl, pending, now);
        }
        state
            .disposed
            .fetch_add(self.held_bytes as u64, Ordering::SeqCst);
        if !self.forwarding {
            self.forwarding = true;
            // The one console line of a restore. The host's dead-serial check counts it.
            let line = format!(
                "[fc-agent] restore: {} progress lines sent to the host over vsock\n",
                self.held.len()
            );
            state
                .enqueued
                .fetch_add(line.len() as u64, Ordering::SeqCst);
            push_console(state, pending, line.as_bytes());
        }
        self.held.clear();
        self.held_bytes = 0;
    }

    /// End the window from the writer's side: close it for the callers and put what it
    /// holds on the console.
    fn end(
        &mut self,
        state: &ConsoleShared,
        ctl: &mut LogCtl,
        pending: &mut VecDeque<u8>,
        now: Instant,
    ) {
        ctl.window = None;
        ctl.acknowledged = false;
        self.spill(state, pending, now);
    }

    /// Everything held goes to the console, in the order it was logged, and the thread is
    /// outside any window. The host stamps a console line when it arrives, which for a
    /// held line is later than it was logged, so each carries how long it was held.
    fn spill(&mut self, state: &ConsoleShared, pending: &mut VecDeque<u8>, now: Instant) {
        for line in self.held.drain(..) {
            let held = format!(
                " held_ms={:.3}\n",
                now.saturating_duration_since(line.logged).as_secs_f64() * 1000.0
            );
            // The suffix is the writer's own bytes, counted in as the pipe's are.
            state
                .enqueued
                .fetch_add(held.len() as u64 - 1, Ordering::SeqCst);
            push_console(state, pending, &line.bytes);
            push_console(state, pending, held.as_bytes());
        }
        push_console(state, pending, &self.inbox);
        self.inbox.clear();
        self.held_bytes = 0;
        self.forwarding = false;
        self.generation = None;
    }
}

/// Where the next console write of `pending` ends: after its first newline, or at its end
/// when it holds none.
///
/// The writer hands the console one line per write. The console is non-blocking and takes
/// what fits, so a write of several lines could end inside one, and another console writer
/// (the kernel, a process writing `/dev/ttyS0`) could land in the rest of it; one line per
/// write splits a line only when the console has less room than that line (#1723).
///
/// The search resumes where it stopped, so a long line written in many short writes is
/// scanned once, not once per write.
#[derive(Default)]
struct LineCursor {
    /// Bytes at the front of `pending` known to hold no newline.
    searched: usize,
    /// Bytes at the front of `pending` that end the current line, once its newline is found.
    line: Option<usize>,
    /// Bytes examined, for tests.
    #[cfg(test)]
    scanned: usize,
}

impl LineCursor {
    /// How many bytes from the front of `pending` the next write may take.
    fn next_len(&mut self, pending: &VecDeque<u8>) -> usize {
        if let Some(line) = self.line {
            return line;
        }
        let from = self.searched.min(pending.len());
        #[cfg(test)]
        {
            self.scanned += pending.len() - from;
        }
        match pending.range(from..).position(|&b| b == b'\n') {
            Some(at) => {
                let line = from + at + 1;
                self.line = Some(line);
                line
            }
            None => {
                self.searched = pending.len();
                pending.len()
            }
        }
    }

    /// `n` bytes left the front of `pending`.
    fn consumed(&mut self, n: usize) {
        self.searched = self.searched.saturating_sub(n);
        self.line = match self.line {
            Some(line) if line > n => Some(line - n),
            _ => None,
        };
    }

    /// `pending` was emptied.
    fn cleared(&mut self) {
        self.searched = 0;
        self.line = None;
    }
}

/// Writer thread poll tick. Bounds how quickly the writer notices a gate
/// transition; pipe data wakes it immediately via POLLIN.
const POLL_TICK_MS: u16 = 100;

/// Bound on each quiesce phase (politeness flush / UART drain). The console
/// works pre-snapshot, so these normally complete in microseconds; the cap
/// only prevents an unexpected wedge from stalling container startup.
const QUIESCE_PHASE_TIMEOUT: Duration = Duration::from_secs(5);

static CONSOLE: OnceLock<Arc<ConsoleShared>> = OnceLock::new();

/// fc-agent's `eprintln!`: the line in one write. Panics if the write fails, as std's does.
pub fn eprint_line(args: std::fmt::Arguments<'_>) {
    if let Err(e) = write_line(&mut std::io::stderr().lock(), args) {
        panic!("failed printing to stderr: {e}");
    }
}

/// Write one line, `args` and its newline, with a single `write_all`. Backs fc-agent's own
/// `eprintln!` (main.rs), which replaces std's.
///
/// std's macro writes a line in pieces, one write per literal and per argument and one for
/// the newline, and the writer thread reads the pipe as the pieces arrive, so a line could
/// reach the console in pieces. Any other console writer (the kernel, a process writing
/// `/dev/ttyS0`) could then land inside it: the serial console test once read
/// `[fc-agent] exec server: accepted connection on vsock port 4998<marker>`, where the exec
/// that wrote the marker raced the line that logged its accept. A pipe write of up to
/// `PIPE_BUF` (4096) bytes is atomic, so a line that size or smaller now leaves the pipe
/// in one read.
pub fn write_line(
    out: &mut impl std::io::Write,
    args: std::fmt::Arguments<'_>,
) -> std::io::Result<()> {
    use std::fmt::Write as _;
    let mut line = String::with_capacity(128);
    // Formatting into a String fails only if a Display impl reports an error, as for std's
    // macros, which then panic.
    line.write_fmt(args)
        .expect("a Display implementation returned an error unexpectedly");
    line.push('\n');
    out.write_all(line.as_bytes())
}

/// Shared state between the writer thread and quiesce callers.
pub struct ConsoleShared {
    console_fd: OwnedFd,
    pipe_rd: OwnedFd,
    /// Kept so the pipe outlives any exotic fd 1/2 juggling; fd 1/2 are dups.
    pipe_wr: OwnedFd,
    /// Bytes the writer thread has taken out of the pipe (responsibility taken).
    enqueued: AtomicU64,
    /// Bytes written to the console, handed to the host as restore progress, or
    /// deliberately dropped.
    /// Invariant: `enqueued - disposed` is what the writer still holds: its pending
    /// buffer and, in a restore log window, the lines held for the host.
    disposed: AtomicU64,
    /// Bytes dropped since the last recovery summary.
    dropped: AtomicU64,
    /// How many quiesce guards are held. While it is above zero the writer thread
    /// must not write to the console. A count, not a flag, because two holds can
    /// overlap: a pre-start snapshot's handshake guard is still held when the host
    /// runs the source boundary's `--prepare-snapshot-network`, and the first of the
    /// two to drop must not reopen the console under the other.
    gate: AtomicUsize,
    /// Held around every console `write()`; `quiesce` locks it once after
    /// setting `gate` so no console write is in flight or can start afterwards.
    /// It also guards the restore log's control state, which the writer reads in the
    /// same critical section as it writes.
    write_lock: Mutex<LogCtl>,
    /// An eventfd the writer polls: a caller that changed [`LogCtl`] writes it, so the
    /// writer acts on the change now and not at its next tick.
    wake: OwnedFd,
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
            state.gate.fetch_sub(1, Ordering::SeqCst);
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
    // Raw `libc::ioctl` below is deliberate: nix has no TIOCOUTQ/FIONREAD wrapper.
    let (pipe_rd, pipe_wr) = nix::unistd::pipe2(OFlag::O_CLOEXEC)?;
    let flags = fcntl(&pipe_rd, FcntlArg::F_GETFL)?;
    fcntl(
        &pipe_rd,
        FcntlArg::F_SETFL(OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK),
    )?;
    // Belt-and-braces: the console fd must be non-blocking even if the caller
    // forgot O_NONBLOCK at open time.
    let cflags = fcntl(&console_fd, FcntlArg::F_GETFL)?;
    fcntl(
        &console_fd,
        FcntlArg::F_SETFL(OFlag::from_bits_truncate(cflags) | OFlag::O_NONBLOCK),
    )?;

    // SAFETY: eventfd takes no pointers; a non-negative return is a new descriptor that
    // nothing else owns.
    let wake = unsafe {
        let fd = libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC);
        if fd < 0 {
            return Err(Errno::last());
        }
        <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd)
    };

    let state = Arc::new(ConsoleShared {
        console_fd,
        pipe_rd,
        pipe_wr,
        enqueued: AtomicU64::new(0),
        disposed: AtomicU64::new(0),
        dropped: AtomicU64::new(0),
        gate: AtomicUsize::new(0),
        write_lock: Mutex::new(LogCtl::default()),
        wake,
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
    let mut cursor = LineCursor::default();
    let mut restore_log = RestoreLog::default();
    let mut fresh: Vec<u8> = Vec::new();

    loop {
        // Poll: always watch the pipe and the wake; watch the console for writability
        // only when there is something to flush and the gate is open. The tick also
        // bounds how late the writer sees a restore log window's deadline.
        let want_write = !pending.is_empty() && state.gate.load(Ordering::SeqCst) == 0;
        {
            let mut fds = [
                PollFd::new(state.pipe_rd.as_fd(), PollFlags::POLLIN),
                PollFd::new(state.wake.as_fd(), PollFlags::POLLIN),
                PollFd::new(state.console_fd.as_fd(), PollFlags::POLLOUT),
            ];
            let nfds = if want_write { 3 } else { 2 };
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
        // The wake carries no data: what changed is read under the lock below.
        let mut woken = [0u8; 8];
        let _ = read(&state.wake, &mut woken);

        // 1) Drain the pipe (non-blocking) so producers can never wedge on it.
        fresh.clear();
        loop {
            match read(&state.pipe_rd, &mut buf) {
                Ok(0) => break, // all write ends closed (cannot happen: we hold one)
                Ok(n) => {
                    state.enqueued.fetch_add(n as u64, Ordering::SeqCst);
                    fresh.extend_from_slice(&buf[..n]);
                    // Route what was read before reading on, so a flood is bounded by
                    // the caps the routing applies and not held here.
                    if n < buf.len() || fresh.len() >= 8 * buf.len() {
                        break;
                    }
                }
                Err(Errno::EAGAIN) => break,
                Err(Errno::EINTR) => continue,
                Err(_) => break,
            }
        }

        // 2) Route what was read, then flush pending to the console, never while
        // gated and never blocking.
        let mut wrote_any = false;
        {
            let mut ctl = state.write_lock.lock().unwrap_or_else(|e| e.into_inner());
            restore_log.route(
                &state,
                &mut ctl,
                &fresh,
                &mut pending,
                &mut cursor,
                Instant::now(),
            );
            // Checked under the lock: after quiesce() sets the gate and cycles
            // this lock, no write can start here.
            if state.gate.load(Ordering::SeqCst) == 0 {
                while !pending.is_empty() {
                    let len = cursor.next_len(&pending);
                    // A line across the ring's wrap is made contiguous, so it is still one
                    // write; that moves the buffer only when a line spans the wrap.
                    if pending.as_slices().0.len() < len {
                        pending.make_contiguous();
                    }
                    let (head, _) = pending.as_slices();
                    match write(state.console_fd.as_fd(), &head[..len]) {
                        Ok(0) => break,
                        Ok(n) => {
                            pending.drain(..n);
                            cursor.consumed(n);
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
                            cursor.cleared();
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
    fn ctl(&self) -> std::sync::MutexGuard<'_, LogCtl> {
        self.write_lock.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Have the writer thread read [`LogCtl`] now.
    fn wake_writer(&self) {
        // A full counter already has the writer's wakeup pending.
        let _ = write(&self.wake, &1u64.to_ne_bytes());
    }

    /// See [`hold_restore_log`].
    fn hold_restore_log(&self, window: Duration) {
        {
            let mut ctl = self.ctl();
            if ctl.sink.is_none() || ctl.window.is_some() {
                return;
            }
            ctl.window = Some(Instant::now() + window);
            ctl.generation += 1;
            ctl.acknowledged = false;
        }
        self.wake_writer();
    }

    /// See [`restore_log_acknowledged`].
    fn restore_log_acknowledged(&self) {
        {
            let mut ctl = self.ctl();
            if ctl.window.is_none() {
                return;
            }
            ctl.acknowledged = true;
        }
        self.wake_writer();
    }

    /// See [`end_restore_log`].
    fn end_restore_log(&self) {
        {
            let mut ctl = self.ctl();
            if ctl.window.take().is_none() {
                return;
            }
            ctl.acknowledged = false;
        }
        self.wake_writer();
    }

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
        // A restore log window ends here: the lines it holds go to the console and
        // drain with the rest below, so the snapshot holds none of them and its
        // clones start without a window.
        self.end_restore_log();

        // Phase 1 (politeness): let already-logged lines reach the console so
        // the quiesce announcement lands before the host pauses the VM. Purely
        // cosmetic for safety — phases 2+3 provide the guarantee.
        // Skipped while another hold is in place: that hold already drained the UART
        // and keeps its pending bytes back, so the flush could only wait out the
        // bound. The pre-start snapshot's handshake hold is in place when the host
        // runs the boundary's prepare, so without this every pre-start snapshot
        // would wait the full bound here.
        let deadline = Instant::now() + phase_timeout;
        while self.gate.load(Ordering::SeqCst) == 0 && !self.flushed() {
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
        self.gate.fetch_add(1, Ordering::SeqCst);
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
                 refusing the snapshot so it cannot capture the UART mid-transmit"
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
/// when the guard drops (the first severed handshake session after the pause,
/// or the handshake's return: ack received, restore detected, or failure).
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

/// Register the connection that carries a restore's progress lines to the host. Without
/// one (a TTY VM has no output connection) no restore log window opens and every line
/// goes to the console.
pub fn set_progress_sink(sink: Arc<dyn ProgressSink>) {
    if let Some(state) = CONSOLE.get() {
        state.ctl().sink = Some(sink);
    }
}

/// A snapshot boundary is behind this guest and a restore of it may be under way: from
/// now until [`RESTORE_LOG_WINDOW`] later, progress lines are kept off the console (see
/// the module doc). Called BEFORE the quiesce guard that held the console across the
/// boundary is dropped, so the lines held behind that guard are the window's first. A
/// window that is already open keeps its deadline.
pub fn hold_restore_log() {
    if let Some(state) = CONSOLE.get() {
        state.hold_restore_log(RESTORE_LOG_WINDOW);
    }
}

/// The host acknowledged the restore: the held progress lines go to it once the output
/// connection is established, and those that follow in the window follow them.
pub fn restore_log_acknowledged() {
    if let Some(state) = CONSOLE.get() {
        state.restore_log_acknowledged();
    }
}

/// End the restore log window, if one is open: the lines it holds, and every later line,
/// go to the console. For a guest that turns out not to be restoring, and before a shutdown.
pub fn end_restore_log() {
    if let Some(state) = CONSOLE.get() {
        state.end_restore_log();
    }
}

/// Have the writer thread look at the restore log again, for the output connection to say
/// it was just established.
pub fn wake_writer() {
    if let Some(state) = CONSOLE.get() {
        state.wake_writer();
    }
}

/// Where the source snapshot boundary asks the running agent to hold its console
/// quiet (see [`request_source_quiesce`]).
pub const SOURCE_GATE_SOCKET: &str = "/run/fc-agent-console-gate.sock";

const SOURCE_QUIESCE_REQUEST: &str = "quiesce";
const SOURCE_RELEASE_REQUEST: &str = "release";

/// Bound on one hold of the source boundary's gate. The host pauses the VM right
/// after `--prepare-snapshot-network` returns and releases the hold from
/// `--resume-snapshot-network`, and a restored clone releases it when it sees its
/// restore epoch; this only ends a hold whose host never came back, so the agent's
/// log lines are delayed, not lost for good. A hold the bound ends while the VM is
/// still running ahead of the pause reopens the window the hold exists to close,
/// so it is far above any prepare-to-pause gap: the host's pause call times out
/// after 30 s.
const SOURCE_HOLD_CAP: Duration = Duration::from_secs(600);

/// How long a one-shot waits for the agent's answer: both quiesce phases at their
/// bound, with room to spare.
const SOURCE_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// The console hold the source snapshot boundary took, with the generation that
/// lets the cap's timer release only the hold it was started for.
struct SourceHold {
    generation: u64,
    _guard: QuiesceGuard,
}

static SOURCE_HOLD: Mutex<Option<SourceHold>> = Mutex::new(None);
static SOURCE_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Quiesce the console and keep it quiet until [`release_source_hold`]. A second
/// hold replaces the first only after the new one holds the gate, so the console
/// never reopens in between.
fn take_source_hold() -> Result<u64, QuiesceError> {
    let guard = quiesce_for_snapshot()?;
    let generation = SOURCE_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    let previous = SOURCE_HOLD
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .replace(SourceHold {
            generation,
            _guard: guard,
        });
    drop(previous);
    Ok(generation)
}

/// Drop the source boundary's hold, if there is one (and, with `only`, only if it
/// is that generation). Returns whether a hold was dropped.
fn release_source_hold(only: Option<u64>) -> bool {
    let mut slot = SOURCE_HOLD.lock().unwrap_or_else(|e| e.into_inner());
    let matches = match (&*slot, only) {
        (Some(hold), Some(generation)) => hold.generation == generation,
        (Some(_), None) => true,
        (None, _) => false,
    };
    let taken = if matches { slot.take() } else { None };
    drop(slot);
    taken.is_some()
}

/// Release the console hold the source boundary took before a snapshot, from a
/// restored clone. The snapshot holds the gate closed, so a clone of it would
/// otherwise keep its console quiet until the cap. The lines held behind it are the
/// restore's first, so the restore log window opens before the hold goes.
pub fn release_source_snapshot_hold() {
    hold_restore_log();
    if release_source_hold(None) {
        eprintln!(
            "[fc-agent] console hold released after restore (held since the snapshot boundary)"
        );
    }
}

/// Answer one request on [`SOURCE_GATE_SOCKET`].
fn answer_source_request(request: &str) -> String {
    match request {
        SOURCE_QUIESCE_REQUEST => match take_source_hold() {
            Ok(generation) => {
                let spawned = std::thread::Builder::new()
                    .name("console-hold-cap".into())
                    .spawn(move || {
                        std::thread::sleep(SOURCE_HOLD_CAP);
                        if release_source_hold(Some(generation)) {
                            eprintln!(
                                "[fc-agent] WARNING: console reopened after {}s: the host \
                                 never resumed or restored after the snapshot boundary",
                                SOURCE_HOLD_CAP.as_secs()
                            );
                        }
                    });
                if spawned.is_err() {
                    // Without the cap a lost resume would keep the console quiet
                    // for good. Refuse the hold, so the host takes no snapshot.
                    release_source_hold(Some(generation));
                    return "err cannot start the hold's cap timer\n".to_string();
                }
                "ok\n".to_string()
            }
            Err(error) => format!("err {error}\n"),
        },
        SOURCE_RELEASE_REQUEST => {
            release_source_hold(None);
            "ok\n".to_string()
        }
        other => format!("err unknown request {other:?}\n"),
    }
}

/// Listen on [`SOURCE_GATE_SOCKET`] for the snapshot boundary's one-shots.
pub fn spawn_source_gate_listener() -> anyhow::Result<()> {
    spawn_source_gate_listener_at(std::path::Path::new(SOURCE_GATE_SOCKET))
}

fn spawn_source_gate_listener_at(path: &std::path::Path) -> anyhow::Result<()> {
    use anyhow::Context;
    use std::io::{BufRead, Read, Write};
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("removing a stale {path:?}")),
    }
    let listener = std::os::unix::net::UnixListener::bind(path)
        .with_context(|| format!("binding the console gate socket {path:?}"))?;
    // A plain thread: the quiesce sleeps in 1 ms steps for up to two phase bounds,
    // and requests come one per snapshot.
    std::thread::Builder::new()
        .name("console-gate".into())
        .spawn(move || {
            for stream in listener.incoming() {
                let mut stream = match stream {
                    Ok(stream) => stream,
                    Err(e) => {
                        eprintln!("[fc-agent] WARNING: console gate socket failed: {e}");
                        std::thread::sleep(Duration::from_millis(100));
                        continue;
                    }
                };
                let _ = stream.set_read_timeout(Some(SOURCE_REQUEST_TIMEOUT));
                let mut line = String::new();
                let mut reader = std::io::BufReader::new(&stream).take(64);
                let request = match reader.read_line(&mut line) {
                    Ok(_) => line.trim().to_string(),
                    Err(_) => continue,
                };
                let answer = answer_source_request(&request);
                let _ = stream.write_all(answer.as_bytes());
            }
        })
        .context("starting the console gate listener")?;
    Ok(())
}

/// Send one request to the agent's console gate and return its answer.
fn source_request_at(path: &std::path::Path, request: &str) -> anyhow::Result<()> {
    use anyhow::Context;
    use std::io::{Read, Write};
    let mut stream = std::os::unix::net::UnixStream::connect(path)
        .with_context(|| format!("connecting to the agent's console gate {path:?}"))?;
    stream.set_read_timeout(Some(SOURCE_REQUEST_TIMEOUT))?;
    stream.set_write_timeout(Some(SOURCE_REQUEST_TIMEOUT))?;
    stream.write_all(format!("{request}\n").as_bytes())?;
    let mut answer = String::new();
    stream
        .take(256)
        .read_to_string(&mut answer)
        .with_context(|| format!("reading the agent's answer to {request:?}"))?;
    match answer.trim() {
        "ok" => Ok(()),
        other => anyhow::bail!("the agent refused {request:?}: {other:?}"),
    }
}

/// From `--prepare-snapshot-network`: ask the running agent to hold its console
/// quiet across the pause the host makes next.
///
/// A snapshot that captures the 8250 mid-transmit gives every restore of it a dead
/// serial console (`quiesce_for_snapshot`). The pre-start snapshot is guarded by the
/// cache handshake; every snapshot of a running VM (a startup snapshot, `fcvm
/// snapshot create`) is paused right after this boundary, and the agent logs into
/// that window: the prepared gate refuses the restore watcher's metadata fetches,
/// and the first five refusals are logged. `Err` means the UART was not proven idle
/// and nothing is held: the prepare fails and the host takes no snapshot.
pub async fn request_source_quiesce() -> anyhow::Result<()> {
    run_source_request(SOURCE_QUIESCE_REQUEST).await
}

/// From `--resume-snapshot-network`: the pause is over, release the hold.
pub async fn request_source_release() -> anyhow::Result<()> {
    run_source_request(SOURCE_RELEASE_REQUEST).await
}

async fn run_source_request(request: &'static str) -> anyhow::Result<()> {
    tokio::task::spawn_blocking(move || {
        source_request_at(std::path::Path::new(SOURCE_GATE_SOCKET), request)
    })
    .await
    .map_err(|e| anyhow::anyhow!("console gate request task failed: {e}"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
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

    /// Writes each piece it is handed into the agent's pipe, waits until the console holds
    /// it, and after the first piece writes a marker straight to the console, as an exec
    /// writing `/dev/ttyS0` would. A line that arrives in pieces then has the marker inside.
    struct RacedPipe<'a> {
        state: &'a Arc<ConsoleShared>,
        console_rd: &'a mut std::fs::File,
        other: &'a std::fs::File,
        seen: &'a mut Vec<u8>,
        sent: usize,
        raced: bool,
    }

    impl std::io::Write for RacedPipe<'_> {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            write_pipe(self.state, buf);
            self.sent += buf.len();
            let sent = self.sent;
            assert!(
                wait_until(
                    || {
                        drain_console(self.console_rd, self.seen);
                        self.seen.len() >= sent
                    },
                    Duration::from_secs(5)
                ),
                "the writer did not forward {sent} bytes"
            );
            if !self.raced {
                self.raced = true;
                let mut other = self.other;
                other.write_all(b"MARKER\n")?;
                self.sent += b"MARKER\n".len();
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// An agent line reaches the console whole when another console writer races it. With
    /// std's `writeln!` the line is three writes and the marker lands after the first, which
    /// is what the serial console test once read.
    #[test]
    fn a_line_reaches_the_console_whole_with_another_writer_racing_it() {
        let (console_wr, mut console_rd) = fake_console();
        let other = std::fs::File::from(console_wr.try_clone().unwrap());
        let state = setup(console_wr).unwrap();
        let mut seen: Vec<u8> = Vec::new();
        let port = 4998;
        {
            let mut raced = RacedPipe {
                state: &state,
                console_rd: &mut console_rd,
                other: &other,
                seen: &mut seen,
                sent: 0,
                raced: false,
            };
            write_line(
                &mut raced,
                format_args!("[fc-agent] exec server: accepted connection on vsock port {port}"),
            )
            .unwrap();
        }
        // The marker went straight to the console after the last write.
        let expected = "[fc-agent] exec server: accepted connection on vsock port 4998\nMARKER\n";
        wait_until(
            || {
                drain_console(&mut console_rd, &mut seen);
                seen.len() >= expected.len()
            },
            Duration::from_secs(5),
        );
        let text = String::from_utf8_lossy(&seen);
        assert_eq!(
            text, expected,
            "an agent line was split by another console writer"
        );
    }

    /// main.rs replaces std's `eprintln!` with one that writes through `eprint_line`, before
    /// any module, so every module's `eprintln!` is the one-write one.
    #[test]
    fn fc_agent_replaces_eprintln_before_its_modules() {
        let main = include_str!("main.rs");
        let defined = main
            .find("macro_rules! eprintln {")
            .expect("main.rs no longer defines fc-agent's eprintln!");
        let first_module = main.find("\nmod ").expect("main.rs declares no module");
        assert!(
            defined < first_module,
            "eprintln! is defined after a module"
        );
        let body = &main[defined..first_module];
        assert_eq!(
            body.matches("$crate::console::eprint_line(format_args!(")
                .count(),
            2
        );
    }

    /// Close the console gate as quiesce does: no write is in flight once this returns.
    fn hold_writer(state: &Arc<ConsoleShared>) {
        state.gate.fetch_add(1, Ordering::SeqCst);
        drop(state.write_lock.lock().unwrap());
    }

    /// A console with less room than the writer's backlog takes whole lines, so another
    /// console writer cannot land inside one (#1723). The writer is held until all six lines
    /// are in its backlog, so the old writer's single write of all 6000 bytes is forced.
    #[test]
    fn a_console_short_of_room_takes_whole_lines() {
        let (console_wr, mut console_rd) = fake_console();
        unsafe {
            libc::fcntl(console_wr.as_raw_fd(), libc::F_SETPIPE_SZ, 4096);
        }
        let other = std::fs::File::from(console_wr.try_clone().unwrap());
        let state = setup(console_wr).unwrap();
        hold_writer(&state);
        for i in 0..6u8 {
            let mut line = vec![b'0' + i; 999];
            line.push(b'\n');
            write_pipe(&state, &line);
        }
        assert!(
            wait_until(
                || state.enqueued.load(Ordering::SeqCst) == 6000,
                Duration::from_secs(5)
            ),
            "the writer did not take the backlog"
        );
        state.gate.fetch_sub(1, Ordering::SeqCst);
        // The console fills: 4096 bytes in one write before, 4000 in four lines now.
        assert!(
            wait_until(
                || state.disposed.load(Ordering::SeqCst) >= 4000,
                Duration::from_secs(5)
            ),
            "the writer never filled the console"
        );
        hold_writer(&state);
        let mut seen: Vec<u8> = Vec::new();
        drain_console(&mut console_rd, &mut seen);
        (&other).write_all(b"MARKER\n").unwrap();
        state.gate.fetch_sub(1, Ordering::SeqCst);
        assert!(
            wait_until(
                || {
                    drain_console(&mut console_rd, &mut seen);
                    seen.len() == 6000 + 7
                },
                Duration::from_secs(5)
            ),
            "{} bytes reached the console",
            seen.len()
        );
        for line in String::from_utf8_lossy(&seen).lines() {
            assert!(
                line == "MARKER" || (line.len() == 999 && !line.contains("MARKER")),
                "a line was split by another console writer: {} bytes, starting {:?}",
                line.len(),
                &line[..line.len().min(8)]
            );
        }
    }

    /// A line with no newline written in many short writes is scanned once.
    #[test]
    fn a_long_line_is_scanned_once() {
        const LEN: usize = 1 << 20;
        let mut pending: VecDeque<u8> = std::iter::repeat_n(b'x', LEN).collect();
        let mut cursor = LineCursor::default();
        while !pending.is_empty() {
            let take = cursor.next_len(&pending).min(4096);
            pending.drain(..take);
            cursor.consumed(take);
        }
        assert_eq!(cursor.scanned, LEN, "bytes examined for a {LEN}-byte line");
        // A newline that arrives later is found where the search stopped.
        pending.extend(b"tail\nnext");
        assert_eq!(cursor.next_len(&pending), 5);
        pending.drain(..5);
        cursor.consumed(5);
        assert_eq!(cursor.next_len(&pending), 4);
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
            state.gate.load(Ordering::SeqCst) == 0,
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

    /// The pre-start handshake's guard is still held when the host runs the source
    /// boundary's prepare, so two holds overlap. The first to drop must not reopen
    /// the console under the other.
    #[test]
    fn overlapping_holds_keep_the_console_gated_until_the_last_drops() {
        let (console_wr, mut console_rd) = fake_console();
        let state = setup(console_wr).unwrap();
        let mut seen: Vec<u8> = Vec::new();

        let first = state
            .quiesce()
            .expect("first quiesce drains a pipe console");
        let second = state
            .quiesce()
            .expect("second quiesce drains a pipe console");
        drop(first);

        write_pipe(&state, b"held-line\n");
        assert!(
            wait_until(
                || state.pipe_backlog() == 0
                    && state.enqueued.load(Ordering::SeqCst)
                        > state.disposed.load(Ordering::SeqCst),
                Duration::from_secs(5)
            ),
            "the writer must take the line into its pending buffer"
        );
        // Several writer ticks: a reopened gate would have flushed by now.
        std::thread::sleep(Duration::from_millis(3 * POLL_TICK_MS as u64));
        drain_console(&mut console_rd, &mut seen);
        assert!(
            seen.is_empty(),
            "the console reopened while a hold was still held: {:?}",
            String::from_utf8_lossy(&seen)
        );

        drop(second);
        assert!(
            wait_until(
                || {
                    drain_console(&mut console_rd, &mut seen);
                    seen.as_slice() == b"held-line\n"
                },
                Duration::from_secs(5)
            ),
            "dropping the last hold must flush, got {:?}",
            String::from_utf8_lossy(&seen)
        );
    }

    /// The source boundary's requests over the agent's console gate socket: a
    /// quiesce takes the hold and answers ok, a release drops it, a stale cap timer
    /// cannot drop a newer hold, and an unknown request is refused.
    #[test]
    fn the_console_gate_socket_takes_and_releases_the_source_hold() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("console-gate.sock");
        spawn_source_gate_listener_at(&path).unwrap();
        let held = || SOURCE_HOLD.lock().unwrap().is_some();

        source_request_at(&path, SOURCE_QUIESCE_REQUEST).expect("quiesce is answered ok");
        assert!(held(), "a quiesce must leave the hold in place");
        source_request_at(&path, SOURCE_RELEASE_REQUEST).expect("release is answered ok");
        assert!(!held(), "a release must drop the hold");

        let older = take_source_hold().unwrap();
        let newer = take_source_hold().unwrap();
        assert!(
            !release_source_hold(Some(older)),
            "the cap timer of a replaced hold must not drop the newer one"
        );
        assert!(held());
        assert!(release_source_hold(Some(newer)));
        assert!(!held());

        let refused = source_request_at(&path, "bogus").unwrap_err();
        assert!(
            format!("{refused:#}").contains("refused"),
            "an unknown request must be refused: {refused:#}"
        );
    }

    /// A second hold taken while one is in place returns at once: the first hold
    /// drained the UART and keeps its pending bytes back, so the flush phase could
    /// only wait out its bound.
    #[test]
    fn a_hold_taken_under_another_does_not_wait_for_the_held_bytes() {
        let (console_wr, _console_rd) = fake_console();
        let state = setup(console_wr).unwrap();
        let first = state
            .quiesce()
            .expect("first quiesce drains a pipe console");
        write_pipe(&state, b"held-line\n");
        assert!(
            wait_until(
                || state.pipe_backlog() == 0
                    && state.enqueued.load(Ordering::SeqCst)
                        > state.disposed.load(Ordering::SeqCst),
                Duration::from_secs(5)
            ),
            "the writer must take the line into its pending buffer"
        );
        let started = Instant::now();
        let second = state
            .quiesce_with_timeout(Duration::from_secs(3))
            .expect("second quiesce drains a pipe console");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the second hold waited {:?} for bytes the first one holds back",
            started.elapsed()
        );
        drop(second);
        drop(first);
    }

    #[test]
    fn test_console_quiesce_noop_without_init() {
        // Must not panic or block when /dev/console was never opened.
        let guard = QuiesceGuard { state: None };
        drop(guard);
    }

    /// The routing decision of the restore log. A line is a progress line only when it is
    /// fc-agent's own and names no failure. Everything the console is the only channel
    /// for goes to it: fc-agent's errors and warnings, a failure site's `VITALS` block, a
    /// panic message, a child's output, a failpoint's line, a line with no prefix.
    #[test]
    fn only_fc_agents_plain_lines_are_restore_progress() {
        for line in [
            "[fc-agent] handling restore (epoch=e)\n",
            "[fc-agent] restore phase=tcp-cleanup epoch=e begin\n",
            "[fc-agent] snapshot network published: destroyed=23 already_gone=0\n",
            "[fc-agent] exec re-registered after restore",
            "[fc-agent] reader[reconnectable]: socket read failed, notifying writer\n",
        ] {
            assert_eq!(classify(line.as_bytes()), Route::Progress, "{line:?}");
        }
        for line in [
            "[fc-agent] FATAL: clone restore failed closed (epoch=e): boom\n",
            "[fc-agent] FATAL ERROR: Container failed to start\n",
            "[fc-agent] ERROR: retiring the snapshot's chronyd panicked: x\n",
            "[fc-agent] Error: mounting FUSE volumes: x\n",
            "[fc-agent] WARNING: journald restart failed: x\n",
            "[fc-agent] VITALS guest begin (container failed to start)\n",
            "MemAvailable: 12 kB\n",
            "thread 'main' panicked at fc-agent/src/restore.rs:1:1:\n",
            "FAILPOINT restore.post_epoch_read reached action=sleep\n",
            "ip: RTNETLINK answers: File exists\n",
            "[fc-agent]\n",
            "[fc-agent]handling restore\n",
            " [fc-agent] handling restore\n",
            "[ctr:out] [fc-agent] handling restore\n",
            "\n",
            "",
        ] {
            assert_eq!(classify(line.as_bytes()), Route::Urgent, "{line:?}");
        }
        for line in [
            "[fcvm-vitals] mem_available_kb=1 pty=0/4096\n",
            "[fcvm-vitals] pileup runnable=12 [fc-agent] FATAL\n",
        ] {
            assert_eq!(classify(line.as_bytes()), Route::Sample, "{line:?}");
        }
    }

    /// The words that make an agent line urgent hold every word the host logs an agent
    /// line at WARN for (`console_line_is_an_agent_error`, src/utils.rs). A word the host
    /// has and the agent lacks would be an error line held back as progress.
    #[test]
    fn the_urgent_words_hold_every_word_the_host_warns_for() {
        let host = include_str!("../../src/utils.rs");
        let function = &host[host
            .find("fn console_line_is_an_agent_error(")
            .expect("the host no longer has console_line_is_an_agent_error")..];
        let function = &function[..function.find("\n}\n").expect("the function ends")];
        let list = &function[function.rfind('[').expect("the word list")..];
        let list = &list[..list.find(']').expect("the word list ends")];
        let words: Vec<&str> = list.split('"').skip(1).step_by(2).collect();
        assert!(
            words.contains(&"FATAL") && words.contains(&"WARNING"),
            "this is not the host's word list: {words:?}"
        );
        for word in words {
            assert!(
                URGENT_WORDS.contains(&word),
                "the host warns for {word:?} and the agent holds such a line back"
            );
        }
    }

    /// A stand-in for the output connection.
    #[derive(Default)]
    struct FakeSink {
        connected: std::sync::atomic::AtomicBool,
        refuse: std::sync::atomic::AtomicBool,
        /// One entry per hand-off: the lines it carried.
        taken: Mutex<Vec<Vec<String>>>,
    }

    impl ProgressSink for FakeSink {
        fn connected(&self) -> bool {
            self.connected.load(Ordering::SeqCst)
        }
        fn send(&self, lines: &[ProgressLine]) -> Result<(), Refused> {
            if self.refuse.load(Ordering::SeqCst) {
                return Err(Refused);
            }
            self.taken.lock().unwrap().push(
                lines
                    .iter()
                    .map(|line| String::from_utf8_lossy(&line.bytes).into_owned())
                    .collect(),
            );
            Ok(())
        }
    }

    impl FakeSink {
        fn connect(&self, state: &ConsoleShared) {
            self.connected.store(true, Ordering::SeqCst);
            state.wake_writer();
        }
        fn lines(&self) -> Vec<String> {
            self.taken.lock().unwrap().concat()
        }
    }

    /// A console whose agent has an output connection, and the fake ends of both.
    fn console_with_sink() -> (Arc<ConsoleShared>, std::fs::File, Arc<FakeSink>) {
        let (console_wr, console_rd) = fake_console();
        let state = setup(console_wr).unwrap();
        let sink = Arc::new(FakeSink::default());
        state.ctl().sink = Some(sink.clone());
        (state, console_rd, sink)
    }

    /// Everything the writer was handed has left it, and this is what the console got.
    fn settled_console(state: &Arc<ConsoleShared>, console_rd: &mut std::fs::File) -> String {
        assert!(
            wait_until(|| state.flushed(), Duration::from_secs(5)),
            "the writer still holds bytes"
        );
        let mut seen = Vec::new();
        drain_console(console_rd, &mut seen);
        String::from_utf8(seen).unwrap()
    }

    /// `console` without the ` held_ms=<n>` each held line ends in on the console, and how
    /// many lines had one.
    fn without_held(console: &str) -> (String, usize) {
        let mut held = 0;
        let lines: Vec<&str> = console
            .split('\n')
            .map(|line| match line.rsplit_once(" held_ms=") {
                Some((text, ms)) if ms.parse::<f64>().is_ok() => {
                    held += 1;
                    text
                }
                _ => line,
            })
            .collect();
        (lines.join("\n"), held)
    }

    const RESTORE_LINES: [&str; 4] = [
        "[fc-agent] detected restore-epoch: e",
        "[fc-agent] handling restore (epoch=e)",
        "[fc-agent] restore phase=tcp-cleanup epoch=e begin",
        "[fc-agent] restore phases complete (epoch=e): exec + egress ready elapsed_ms=22.6",
    ];

    /// Put the open window's deadline in the past, as time passing would. The tests open
    /// their windows for a minute, so none ends because the test itself was slow.
    fn pass_the_deadline(state: &Arc<ConsoleShared>) {
        let mut ctl = state.ctl();
        assert!(ctl.window.is_some(), "no window is open");
        ctl.window = Some(Instant::now());
        drop(ctl);
        state.wake_writer();
    }

    fn write_lines(state: &Arc<ConsoleShared>, lines: &[&str]) {
        for line in lines {
            write_pipe(state, format!("{line}\n").as_bytes());
        }
    }

    /// A restore's progress lines do not reach the console: the lines held behind the
    /// snapshot's quiesce guard and those logged after it are handed to the output
    /// connection, in order and in one hand-off, once the host has acknowledged and the
    /// connection is up. The console gets one line.
    ///
    /// RED WITHOUT THE WINDOW (`hold_restore_log` returning at once): the console got all
    /// four lines and the output connection none.
    #[test]
    fn a_restores_progress_lines_go_to_the_host_and_one_line_to_the_console() {
        let (state, mut console_rd, sink) = console_with_sink();
        // The snapshot was taken under a hold, and the clone logs behind it.
        let guard = state.quiesce().expect("a pipe console drains");
        write_lines(&state, &RESTORE_LINES[..2]);
        assert!(wait_until(
            || state.enqueued.load(Ordering::SeqCst) > 0 && state.pipe_backlog() == 0,
            Duration::from_secs(5)
        ));
        state.hold_restore_log(Duration::from_secs(60));
        drop(guard);
        write_lines(&state, &RESTORE_LINES[2..]);

        // Neither the acknowledgement alone nor the connection alone hands them over.
        state.restore_log_acknowledged();
        std::thread::sleep(Duration::from_millis(3 * POLL_TICK_MS as u64));
        assert!(sink.lines().is_empty(), "handed over with no connection");
        sink.connect(&state);
        assert!(
            wait_until(|| !sink.lines().is_empty(), Duration::from_secs(5)),
            "the lines never reached the output connection"
        );
        assert_eq!(sink.lines(), RESTORE_LINES);
        assert_eq!(
            sink.taken.lock().unwrap().len(),
            1,
            "more than one hand-off"
        );
        assert_eq!(
            settled_console(&state, &mut console_rd),
            "[fc-agent] restore: 4 progress lines sent to the host over vsock\n"
        );
    }

    /// Progress lines wait for the acknowledgement even with the connection up: a restore
    /// that fails after them must find them still here, for the console.
    #[test]
    fn nothing_is_handed_over_before_the_host_acknowledged() {
        let (state, mut console_rd, sink) = console_with_sink();
        sink.connect(&state);
        state.hold_restore_log(Duration::from_secs(60));
        write_lines(&state, &RESTORE_LINES[..2]);
        std::thread::sleep(Duration::from_millis(3 * POLL_TICK_MS as u64));
        assert!(sink.lines().is_empty(), "handed over unacknowledged");
        write_lines(
            &state,
            &["[fc-agent] FATAL: clone restore failed closed (epoch=e): boom"],
        );
        assert_eq!(
            without_held(&settled_console(&state, &mut console_rd)),
            (
                format!(
                    "{}\n{}\n[fc-agent] FATAL: clone restore failed closed (epoch=e): boom\n",
                    RESTORE_LINES[0], RESTORE_LINES[1]
                ),
                2
            )
        );
        assert!(sink.lines().is_empty());
    }

    /// A line that is not a progress line puts the held lines on the console before
    /// itself, and the window is over: the lines after it go to the console as well,
    /// acknowledged and connected or not.
    #[test]
    fn an_urgent_line_puts_the_held_lines_on_the_console_before_itself() {
        for urgent in [
            "[fc-agent] WARNING: could not give the clone its own NFS client identity: x",
            "thread 'tokio-runtime-worker' panicked at fc-agent/src/restore.rs:1:1:",
        ] {
            let (state, mut console_rd, sink) = console_with_sink();
            state.hold_restore_log(Duration::from_secs(60));
            write_lines(&state, &RESTORE_LINES[..2]);
            write_lines(&state, &[urgent]);
            write_lines(&state, &RESTORE_LINES[2..]);
            state.restore_log_acknowledged();
            sink.connect(&state);
            // The two lines held before it say how long they were held; the lines after
            // it were never held.
            assert_eq!(
                without_held(&settled_console(&state, &mut console_rd)),
                (
                    format!(
                        "{}\n{}\n{urgent}\n{}\n{}\n",
                        RESTORE_LINES[0], RESTORE_LINES[1], RESTORE_LINES[2], RESTORE_LINES[3]
                    ),
                    2
                )
            );
            assert!(sink.lines().is_empty(), "{:?}", sink.lines());
        }
    }

    /// A restore that is not acknowledged, or whose output connection is not up, when the
    /// window ends gets its lines on the console then, an unfinished line included, and
    /// later lines go straight there. A hang shows where it stopped within the window.
    #[test]
    fn a_restore_that_does_not_finish_in_the_window_gets_its_lines_on_the_console() {
        for acknowledged in [false, true] {
            let (state, mut console_rd, sink) = console_with_sink();
            state.hold_restore_log(Duration::from_secs(60));
            write_lines(&state, &RESTORE_LINES[..3]);
            write_pipe(&state, b"[fc-agent] an unfinished");
            if acknowledged {
                state.restore_log_acknowledged();
            }
            // The writer has taken every byte, and a tick later the console has none.
            let written: usize = RESTORE_LINES[..3]
                .iter()
                .map(|l| l.len() + 1)
                .sum::<usize>()
                + "[fc-agent] an unfinished".len();
            assert!(wait_until(
                || state.enqueued.load(Ordering::SeqCst) == written as u64,
                Duration::from_secs(5)
            ));
            std::thread::sleep(Duration::from_millis(POLL_TICK_MS as u64));
            let mut seen = Vec::new();
            drain_console(&mut console_rd, &mut seen);
            assert!(seen.is_empty(), "the console got lines inside the window");
            std::thread::sleep(Duration::from_millis(20));
            pass_the_deadline(&state);
            let expected = format!(
                "{}\n{}\n{}\n[fc-agent] an unfinished",
                RESTORE_LINES[0], RESTORE_LINES[1], RESTORE_LINES[2]
            );
            assert!(
                wait_until(
                    || {
                        drain_console(&mut console_rd, &mut seen);
                        seen.ends_with(b"[fc-agent] an unfinished")
                    },
                    Duration::from_secs(5)
                ),
                "the window's end did not put the lines on the console"
            );
            let console = String::from_utf8_lossy(&seen).into_owned();
            assert_eq!(without_held(&console), (expected, 3));
            // Each line says how long it was held: the tick and the 20 ms above at least.
            for line in console.lines().take(3) {
                let held: f64 = line.rsplit_once(" held_ms=").unwrap().1.parse().unwrap();
                assert!(held >= 20.0, "{line:?}");
            }
            // The window is over: a connection that comes up now carries nothing.
            sink.connect(&state);
            write_pipe(
                &state,
                b" line\n[fc-agent] exec re-registered after restore\n",
            );
            assert_eq!(
                settled_console(&state, &mut console_rd),
                " line\n[fc-agent] exec re-registered after restore\n"
            );
            assert!(sink.lines().is_empty());
        }
    }

    /// A hand-off the output connection refuses (its queue is full or closed) loses
    /// nothing: the lines go to the console.
    #[test]
    fn a_refused_hand_off_puts_the_lines_on_the_console() {
        let (state, mut console_rd, sink) = console_with_sink();
        sink.refuse.store(true, Ordering::SeqCst);
        sink.connect(&state);
        state.hold_restore_log(Duration::from_secs(60));
        write_lines(&state, &RESTORE_LINES);
        state.restore_log_acknowledged();
        assert_eq!(
            without_held(&settled_console(&state, &mut console_rd)),
            (RESTORE_LINES.map(|line| format!("{line}\n")).concat(), 4)
        );
        assert!(state.ctl().window.is_none(), "the window stayed open");
    }

    /// After the hand-off, the window's later progress lines follow on the output
    /// connection (the restore's background jobs log after the acknowledgement), with no
    /// second console line. When the window ends they go to the console again.
    #[test]
    fn progress_lines_after_the_hand_off_follow_it_until_the_window_ends() {
        let (state, mut console_rd, sink) = console_with_sink();
        sink.connect(&state);
        state.hold_restore_log(Duration::from_secs(60));
        write_lines(&state, &RESTORE_LINES);
        state.restore_log_acknowledged();
        assert!(wait_until(
            || sink.lines().len() == RESTORE_LINES.len(),
            Duration::from_secs(5)
        ));
        write_lines(&state, &["[fc-agent] journald restarted after restore"]);
        assert!(
            wait_until(
                || sink.lines().len() == RESTORE_LINES.len() + 1,
                Duration::from_secs(5)
            ),
            "a progress line after the hand-off did not follow it"
        );
        assert_eq!(
            settled_console(&state, &mut console_rd),
            "[fc-agent] restore: 4 progress lines sent to the host over vsock\n"
        );
        pass_the_deadline(&state);
        assert!(
            wait_until(|| state.ctl().window.is_none(), Duration::from_secs(5)),
            "the window never ended"
        );
        write_lines(
            &state,
            &["[fc-agent] exec server: accepted connection on vsock port 4998"],
        );
        assert_eq!(
            settled_console(&state, &mut console_rd),
            "[fc-agent] exec server: accepted connection on vsock port 4998\n"
        );
        assert_eq!(sink.lines().len(), RESTORE_LINES.len() + 1);
    }

    /// A `[fcvm-vitals]` sample reaches the console inside a window and leaves it open.
    #[test]
    fn a_vitals_sample_reaches_the_console_inside_the_window() {
        let (state, mut console_rd, sink) = console_with_sink();
        state.hold_restore_log(Duration::from_secs(60));
        write_lines(
            &state,
            &[
                RESTORE_LINES[0],
                "[fcvm-vitals] pileup runnable=12",
                RESTORE_LINES[1],
            ],
        );
        let mut seen = Vec::new();
        assert!(
            wait_until(
                || {
                    drain_console(&mut console_rd, &mut seen);
                    seen.as_slice() == b"[fcvm-vitals] pileup runnable=12\n"
                },
                Duration::from_secs(5)
            ),
            "the sample did not reach the console: {:?}",
            String::from_utf8_lossy(&seen)
        );
        state.restore_log_acknowledged();
        sink.connect(&state);
        assert!(wait_until(
            || sink.lines().len() == 2,
            Duration::from_secs(5)
        ));
        assert_eq!(sink.lines(), RESTORE_LINES[..2]);
    }

    /// A snapshot's quiesce ends the window: the held lines drain to the console before
    /// the hold, and the snapshot holds neither them nor an open window.
    #[test]
    fn a_snapshot_quiesce_ends_the_restore_log_window() {
        let (state, mut console_rd, sink) = console_with_sink();
        state.hold_restore_log(Duration::from_secs(60));
        write_lines(&state, &RESTORE_LINES[..2]);
        assert!(wait_until(
            || state.enqueued.load(Ordering::SeqCst) > 0 && state.pipe_backlog() == 0,
            Duration::from_secs(5)
        ));
        let guard = state.quiesce().expect("a pipe console drains");
        let mut seen = Vec::new();
        drain_console(&mut console_rd, &mut seen);
        assert_eq!(
            without_held(&String::from_utf8_lossy(&seen)),
            (format!("{}\n{}\n", RESTORE_LINES[0], RESTORE_LINES[1]), 2),
            "the held lines did not drain before the hold"
        );
        assert!(state.ctl().window.is_none());
        drop(guard);
        assert!(sink.lines().is_empty());
    }

    /// `end_restore_log` (a guest that is not restoring after all, a shutdown) puts the
    /// held lines on the console.
    #[test]
    fn ending_the_window_puts_the_held_lines_on_the_console() {
        let (state, mut console_rd, sink) = console_with_sink();
        state.hold_restore_log(Duration::from_secs(60));
        write_lines(&state, &RESTORE_LINES[..2]);
        assert!(wait_until(
            || state.enqueued.load(Ordering::SeqCst) > 0 && state.pipe_backlog() == 0,
            Duration::from_secs(5)
        ));
        state.end_restore_log();
        write_lines(&state, &["[fc-agent] shutting down VM (exit_code=0)"]);
        sink.connect(&state);
        assert_eq!(
            without_held(&settled_console(&state, &mut console_rd)),
            (
                format!(
                    "{}\n{}\n[fc-agent] shutting down VM (exit_code=0)\n",
                    RESTORE_LINES[0], RESTORE_LINES[1]
                ),
                2
            )
        );
        assert!(sink.lines().is_empty());
    }

    /// With no output connection registered (a TTY VM) no window opens: every line goes
    /// to the console as it is logged.
    #[test]
    fn without_an_output_connection_no_window_opens() {
        let (console_wr, mut console_rd) = fake_console();
        let state = setup(console_wr).unwrap();
        state.hold_restore_log(Duration::from_secs(60));
        assert!(state.ctl().window.is_none());
        write_lines(&state, &RESTORE_LINES);
        assert_eq!(
            settled_console(&state, &mut console_rd),
            RESTORE_LINES.map(|line| format!("{line}\n")).concat()
        );
    }

    /// Held lines are bounded: past the cap the window ends and they go to the console,
    /// which has its own bound.
    #[test]
    fn held_lines_past_the_cap_end_the_window() {
        let (state, mut console_rd, _sink) = console_with_sink();
        state.hold_restore_log(Duration::from_secs(60));
        let line = format!("[fc-agent] {}\n", "x".repeat(1000));
        let lines = HELD_CAP / line.len() + 2;
        let mut seen = Vec::new();
        for _ in 0..lines {
            write_pipe(&state, line.as_bytes());
            drain_console(&mut console_rd, &mut seen);
        }
        let all_arrived = |seen: &[u8]| {
            let (text, _) = without_held(&String::from_utf8_lossy(seen));
            text.len() == lines * line.len()
        };
        assert!(
            wait_until(
                || {
                    drain_console(&mut console_rd, &mut seen);
                    all_arrived(&seen)
                },
                Duration::from_secs(10)
            ),
            "the console got {} bytes of {} lines",
            seen.len(),
            lines
        );
        assert!(state.ctl().window.is_none());
        assert_eq!(state.dropped.load(Ordering::SeqCst), 0);
        assert!(wait_until(|| state.flushed(), Duration::from_secs(5)));
    }

    /// A restore opens the window before the hold that kept the console quiet across the
    /// snapshot goes, at both places a clone's hold is released, so no line held behind
    /// it reaches the console in between.
    #[test]
    fn the_window_opens_before_the_snapshots_hold_is_released() {
        let squeezed = |source: &str| source.split_whitespace().collect::<String>();
        let console = squeezed(include_str!("console.rs"));
        let release = &console[console
            .find("pubfnrelease_source_snapshot_hold(){")
            .expect("no release_source_snapshot_hold")..];
        assert!(
            release.find("hold_restore_log();").expect("no window")
                < release
                    .find("release_source_hold(None)")
                    .expect("no release"),
            "the clone's hold is released before the window opens"
        );
        let container = squeezed(include_str!("container.rs"));
        let release = &container[container
            .find("letmutrelease_console=|why:&str|{")
            .expect("no release_console in the cache handshake")..];
        assert!(
            release
                .find("crate::console::hold_restore_log();")
                .expect("no window")
                < release.find("drop(gate);").expect("no release"),
            "the handshake's hold is released before the window opens"
        );
        // A guest the handshake finds is not a restored clone ends the window.
        assert!(container
            .contains("ifresult!=CacheResult::WarmStart{crate::console::end_restore_log();}"));
    }
}
