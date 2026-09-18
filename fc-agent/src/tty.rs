//! Framed exec sessions for fc-agent.
//!
//! One implementation runs the command for every `fcvm exec` mode and for
//! `podman run -it`: on a PTY (-t) or on pipes, with stdin forwarded (-i) or
//! tied to /dev/null. Output travels to the host as exec-proto frames, so it is
//! byte-exact, and without a PTY stdout and stderr stay separate streams.
//!
//! The behaviour follows `podman exec`:
//! - the session ends when the command exits, even if a background process it
//!   started still holds the output open;
//! - end of input on the host closes the command's stdin (pipes only, a PTY has
//!   no half-close);
//! - a PTY starts at the size of the host's terminal and follows its resizes;
//! - a command killed by signal N exits 128+N, a command that cannot be found
//!   exits 127, and one that cannot be run exits 126.
//!
//! Deliberate difference: when the host side of an `fcvm exec` goes away, the
//! command's process group is killed (#636), so a host-side timeout cannot
//! leak a guest command. `podman exec` leaves it running. Two cases are
//! exempt: a detached command (-d), and the VM's own `podman run -it`
//! console, whose connection a snapshot drops and whose container a snapshot
//! must not disturb. For a container exec the group is the guest's
//! `podman exec` client, and the process inside the container outlives it, as
//! it does under podman.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::ExitStatusExt;
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use exec_proto::Message;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{watch, Notify};

use crate::vsock::AsyncFdStream;

/// Vsock port for TTY I/O (used by podman run -it)
pub const TTY_VSOCK_PORT: u32 = 4996;

/// Exit code when the command was not found (`podman exec` uses the same).
const EXIT_NOT_FOUND: i32 = 127;
/// Exit code when the command exists but could not be run.
const EXIT_CANNOT_RUN: i32 = 126;

/// What to run and how to attach it.
pub struct SessionSpec {
    /// Program and arguments.
    pub argv: Vec<String>,
    /// Extra environment for the command.
    pub env: Vec<(String, String)>,
    /// Attach a PTY (-t).
    pub tty: bool,
    /// Forward the host's stdin (-i). Without it stdin is /dev/null.
    pub interactive: bool,
    /// Size the PTY before the command starts. Later sizes arrive as frames.
    pub size: Option<exec_proto::TtySize>,
    /// Start the PTY in raw mode. Set when the command is a podman client
    /// running with -t: the container's own PTY does the echo, line editing
    /// and signal keys, and this PTY only has to carry bytes to it. podman
    /// switches its terminal to raw itself, but input that arrives first would
    /// be echoed and edited twice.
    pub raw_pty: bool,
    /// Working directory for the command. One that does not exist fails the
    /// start like a missing command, as `podman exec -w` does.
    pub workdir: Option<String>,
    /// `USER[:GROUP]` to run the command as, names or numbers.
    pub user: Option<String>,
    /// Start the command in its own session with no stdio, report its pid, and
    /// return without waiting for it.
    pub detach: bool,
    /// Kill the command when the host side of the connection goes away (#636).
    /// True for `fcvm exec`. False for the VM's own `podman run -it` console:
    /// a snapshot resets vsock and drops that connection, and the container
    /// it carries must not be disturbed by a snapshot.
    pub kill_on_disconnect: bool,
}

/// Connect to the host's TTY listener and run `command` on that connection.
///
/// Used by `podman run -it`, where fc-agent initiates the connection.
pub async fn run_with_pty(command: &[String], tty: bool, interactive: bool) -> i32 {
    if command.is_empty() {
        eprintln!("[fc-agent] tty: empty command");
        return 1;
    }
    let conn = match crate::vsock::connect_blocking(crate::vsock::HOST_CID, TTY_VSOCK_PORT) {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("[fc-agent] tty: failed to connect vsock: {:#}", e);
            return 1;
        }
    };
    let spec = SessionSpec {
        argv: command.to_vec(),
        env: Vec::new(),
        tty,
        interactive,
        // A host with a terminal on stdin sends its size as the first frame.
        size: None,
        // `command` is `podman run -t`.
        raw_pty: true,
        workdir: None,
        user: None,
        detach: false,
        kill_on_disconnect: false,
    };
    run_session(conn, spec).await
}

/// Run one command on an established connection and return its exit code.
///
/// The connection is closed when this returns.
pub async fn run_session(conn: OwnedFd, spec: SessionSpec) -> i32 {
    // The command must not inherit the connection: the host would never see it close.
    if let Err(e) = set_cloexec(conn.as_raw_fd()) {
        eprintln!("[fc-agent] exec: cannot set close-on-exec on the connection: {e}");
        return 1;
    }
    let conn = match AsyncFdStream::new(conn) {
        Ok(conn) => conn,
        Err(e) => {
            eprintln!("[fc-agent] exec: cannot register the connection: {e}");
            return 1;
        }
    };
    let (writer, writer_task) = FrameWriter::start(conn.handle());

    if spec.detach {
        let code = run_detached(&spec, &writer).await;
        writer.finish(writer_task).await;
        return code;
    }

    // The window is the first frame of every interactive session, ahead of
    // any output and of a failure to start the command. The host reads output
    // that arrives before it as the mark of an fc-agent without flow control.
    let window = spec
        .interactive
        .then(|| Arc::new(AtomicI64::new(i64::from(STDIN_WINDOW))));
    if window.is_some() {
        let _ = send(&writer, &Message::StdinWindow(STDIN_WINDOW)).await;
    }

    let Spawned {
        mut child,
        outputs,
        stdin,
        pty,
    } = match spawn(&spec) {
        Ok(spawned) => spawned,
        Err(e) => {
            let code = if e.kind() == std::io::ErrorKind::NotFound {
                EXIT_NOT_FOUND
            } else {
                EXIT_CANNOT_RUN
            };
            let text = spawn_error_text(&spec.argv[0], &e);
            let frame = if spec.tty {
                Message::Data(text.into_bytes())
            } else {
                Message::Stderr(text.into_bytes())
            };
            let _ = send(&writer, &frame).await;
            let _ = send(&writer, &Message::Exit(code)).await;
            writer.finish(writer_task).await;
            return code;
        }
    };
    debug_assert_eq!(stdin.is_some(), spec.interactive);

    let (exited_tx, exited_rx) = watch::channel(false);
    let pumps: Vec<_> = outputs
        .into_iter()
        .map(|output| {
            tokio::spawn(pump(
                output,
                writer.clone(),
                exited_rx.clone(),
                // The console's command runs on without a host. Keep reading
                // its output so it never blocks on a full pipe or PTY.
                !spec.kill_on_disconnect,
            ))
        })
        .collect();

    // Reading frames is split from applying them, so a command that does not
    // read its stdin blocks only the applier. The reader never stops: forwarded
    // input is bounded by the window granted here, not by a full connection,
    // so a read is always outstanding to notice the host going away. A host
    // hangup does not cross a vsock connection that has backed up.
    //
    // The queue between the two holds stdin only, which the window bounds.
    let peer_gone = Arc::new(Notify::new());
    let (input_tx, input_rx) = tokio::sync::mpsc::unbounded_channel();
    let helpers = vec![
        tokio::spawn(read_frames(
            conn.handle(),
            input_tx,
            window.clone(),
            pty,
            peer_gone.clone(),
        )),
        tokio::spawn(apply_input(input_rx, stdin, window, writer.clone())),
    ];
    let stop_helpers = |helpers: Vec<tokio::task::JoinHandle<()>>| async move {
        for helper in &helpers {
            helper.abort();
        }
        // Wait for them, so their connection handles are released and the fd
        // closes when this function returns.
        for helper in helpers {
            let _ = helper.await;
        }
    };

    let status = tokio::select! {
        status = child.wait() => status,
        () = peer_gone.notified() => {
            if spec.kill_on_disconnect {
                // Nobody can receive output or the exit code. Kill the whole
                // process group; the child is its leader in both modes
                // (process_group(0) for pipes, setsid for a PTY). The child is
                // not reaped yet, so its pid cannot have been reused.
                if let Some(pid) = child.id() {
                    unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
                }
                let _ = child.start_kill();
                let _ = child.wait().await;
                for pump in pumps {
                    pump.abort();
                    let _ = pump.await;
                }
                stop_helpers(helpers).await;
                // Nobody is listening, so what is queued is dropped. Waiting for
                // the task releases its handle on the connection.
                writer_task.abort();
                let _ = writer_task.await;
                return EXIT_CANNOT_RUN;
            }
            // The console: the host is gone and the container is not ours to stop.
            child.wait().await
        }
    };

    // The command has exited, so everything it wrote is already in the pipe or
    // PTY. The pumps forward that and stop; they do not wait for a background
    // process that inherited the output.
    let _ = exited_tx.send(true);
    for pump in pumps {
        let _ = pump.await;
    }
    stop_helpers(helpers).await;

    let exit_code = match status {
        Ok(status) => status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)),
        Err(e) => {
            eprintln!("[fc-agent] exec: wait failed: {e}");
            1
        }
    };
    let _ = send(&writer, &Message::Exit(exit_code)).await;
    writer.finish(writer_task).await;
    exit_code
}

/// The one writer of the connection. Everything bound for the host is handed
/// to it as a whole frame.
///
/// Handing a frame over is cancel-safe: a sender that is aborted either queued
/// its frame or did not. A socket write is not: cut off part-way it leaves a
/// torn frame, and the Exit that follows would be misread. The session aborts
/// its helper tasks when the command exits, and one of them sends window
/// grants, so no sender may write to the socket itself.
///
/// The queue is short, so a host that reads slowly still slows the pumps down.
#[derive(Clone)]
struct FrameWriter {
    frames: tokio::sync::mpsc::Sender<Vec<u8>>,
}

impl FrameWriter {
    /// The task ends when every `FrameWriter` is dropped and the queue is
    /// written out, or when the host stops taking frames.
    fn start(mut conn: AsyncFdStream) -> (Self, tokio::task::JoinHandle<()>) {
        let (frames, mut queued) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
        let task = tokio::spawn(async move {
            while let Some(frame) = queued.recv().await {
                if let Err(e) = conn.write_all(&frame).await {
                    eprintln!("[fc-agent] exec: writing to the host failed: {e}");
                    return; // dropping the queue fails every later send
                }
            }
        });
        (Self { frames }, task)
    }

    /// Write out everything queued, Exit included, and release the
    /// connection. Every other sender must be gone: the task ends when the
    /// last one is dropped.
    async fn finish(self, task: tokio::task::JoinHandle<()>) {
        drop(self);
        let _ = task.await;
    }
}

/// Which frame type an output stream is sent as.
#[derive(Clone, Copy)]
enum OutputStream {
    /// stdout, or everything the PTY produced
    Data,
    Stderr,
}

impl OutputStream {
    fn frame(self, bytes: &[u8]) -> Message {
        match self {
            OutputStream::Data => Message::Data(bytes.to_vec()),
            OutputStream::Stderr => Message::Stderr(bytes.to_vec()),
        }
    }
}

trait Source: AsyncRead + AsRawFd + Unpin + Send {}
impl<T: AsyncRead + AsRawFd + Unpin + Send> Source for T {}

struct Output {
    source: Box<dyn Source>,
    stream: OutputStream,
    /// A PTY master and a pipe reach "everything delivered" differently.
    is_pty: bool,
}

struct Spawned {
    child: tokio::process::Child,
    outputs: Vec<Output>,
    /// Where forwarded stdin goes; `None` without -i.
    stdin: Option<StdinSink>,
    /// The PTY master, for applying resizes; `None` without -t.
    pty: Option<AsyncFdStream>,
}

enum StdinSink {
    /// Dropping it closes the command's stdin.
    Pipe(tokio::process::ChildStdin),
    /// A PTY has no half-close, so end of input leaves it open.
    Pty(AsyncFdStream),
}

/// Start the command with no stdio in a session of its own, and report its pid.
///
/// Nothing is attached, so the host going away does not end the command. That
/// is the point of -d, and the one case where #636 does not apply.
async fn run_detached(spec: &SessionSpec, writer: &FrameWriter) -> i32 {
    let started = base_command(spec).and_then(|(mut cmd, identity)| {
        // With -t the command gets a terminal of its own and no TERM, which is
        // what `podman exec -d -t` gives it. Without -t it gets /dev/null.
        let terminal = if spec.tty {
            let (master, slave) = open_pty()?;
            if let Some(size) = spec.size {
                set_winsize(master.as_raw_fd(), size)?;
            }
            cmd.stdin(Stdio::from(slave.try_clone()?));
            cmd.stdout(Stdio::from(slave.try_clone()?));
            cmd.stderr(Stdio::from(slave));
            if !spec.env.iter().any(|(key, _)| key == "TERM") {
                cmd.env_remove("TERM");
            }
            Some(master)
        } else {
            cmd.stdin(Stdio::null());
            cmd.stdout(Stdio::null());
            cmd.stderr(Stdio::null());
            None
        };
        let has_terminal = terminal.is_some();
        unsafe {
            cmd.pre_exec(move || {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if has_terminal && libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        run_as(&mut cmd, identity);
        let child = cmd.spawn()?;
        // Command holds the parent's copies of the terminal's slave side.
        drop(cmd);
        Ok((child, terminal))
    });
    let started = started.map(|(child, terminal)| {
        let pid = child.id().unwrap_or(0);
        match terminal {
            Some(master) => hold_detached_terminal(master, child),
            // tokio reaps a dropped child in the background once it exits.
            None => drop(child),
        }
        pid
    });
    let (frame, code) = match started {
        Ok(pid) => (Message::Data(format!("{pid}\n").into_bytes()), 0),
        Err(e) => {
            let code = if e.kind() == std::io::ErrorKind::NotFound {
                EXIT_NOT_FOUND
            } else {
                EXIT_CANNOT_RUN
            };
            let text = spawn_error_text(&spec.argv[0], &e);
            (Message::Stderr(text.into_bytes()), code)
        }
    };
    let _ = send(writer, &frame).await;
    let _ = send(writer, &Message::Exit(code)).await;
    code
}

/// The identity a command runs as, resolved before fork so the child only
/// makes system calls.
struct Identity {
    uid: libc::uid_t,
    gid: libc::gid_t,
    groups: Vec<libc::gid_t>,
    home: Option<std::path::PathBuf>,
}

/// Resolve `USER[:GROUP]` the way `podman exec -u` does: a known user brings
/// its group, supplementary groups and home; an unknown number runs with gid
/// 0; an explicit group replaces the supplementary groups.
fn resolve_identity(spec: &str) -> std::io::Result<Identity> {
    use nix::unistd::{Gid, Group, Uid, User};
    let invalid = |what: String| std::io::Error::new(std::io::ErrorKind::InvalidInput, what);
    let (user_part, group_part) = match spec.split_once(':') {
        Some((user, group)) => (user, Some(group)),
        None => (spec, None),
    };

    let known = match user_part.parse::<u32>() {
        Ok(uid) => User::from_uid(Uid::from_raw(uid)).map_err(std::io::Error::from)?,
        Err(_) => Some(
            User::from_name(user_part)
                .map_err(std::io::Error::from)?
                .ok_or_else(|| invalid(format!("unable to find user {user_part}")))?,
        ),
    };
    let uid = match &known {
        Some(user) => user.uid.as_raw(),
        None => user_part.parse::<u32>().expect("parsed above"),
    };

    let (gid, groups) = match group_part {
        Some(group) => {
            let gid = match group.parse::<u32>() {
                Ok(gid) => gid,
                Err(_) => Group::from_name(group)
                    .map_err(std::io::Error::from)?
                    .ok_or_else(|| invalid(format!("unable to find group {group}")))?
                    .gid
                    .as_raw(),
            };
            (gid, vec![gid])
        }
        None => match &known {
            Some(user) => {
                let name = std::ffi::CString::new(user.name.as_str())
                    .map_err(|_| invalid(format!("user name {:?} contains NUL", user.name)))?;
                let groups = nix::unistd::getgrouplist(&name, Gid::from_raw(user.gid.as_raw()))
                    .map_err(std::io::Error::from)?
                    .into_iter()
                    .map(Gid::as_raw)
                    .collect();
                (user.gid.as_raw(), groups)
            }
            None => (0, vec![0]),
        },
    };
    Ok(Identity {
        uid,
        gid,
        groups,
        home: known.map(|user| user.dir),
    })
}

/// Keep a detached command's terminal open and empty for as long as the
/// command lives. Nobody reads it, but closing the master would hang the
/// command up, and a full terminal would block its writes.
///
/// The command leads the terminal's session, so the kernel hangs the terminal
/// up when it exits, whoever else still holds it. The read then fails and the
/// command is reaped at once; it does not wait as a zombie for a descendant.
fn hold_detached_terminal(master: OwnedFd, mut child: tokio::process::Child) {
    tokio::spawn(async move {
        match AsyncFdStream::new(master) {
            Ok(mut master) => {
                let mut buf = vec![0u8; 4096];
                loop {
                    match tokio::io::AsyncReadExt::read(&mut master, &mut buf).await {
                        Ok(n) if n > 0 => {}
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                        // Hung up: the command has exited.
                        _ => break,
                    }
                }
            }
            Err(e) => eprintln!("[fc-agent] exec: cannot hold a detached terminal: {e}"),
        }
        let _ = child.wait().await;
    });
}

/// The command with its arguments, environment and directory, plus the
/// identity it should run as. The caller applies the identity with
/// [`run_as`] after its own `pre_exec` setup, so that the drop is the last
/// thing the child does before exec.
fn base_command(
    spec: &SessionSpec,
) -> std::io::Result<(tokio::process::Command, Option<Identity>)> {
    let mut cmd = tokio::process::Command::new(&spec.argv[0]);
    cmd.args(&spec.argv[1..]);
    // fc-agent's own TERM describes the serial console, not this session: a
    // PTY gets podman's default, anything else gets none. Set first, so the
    // request's -e TERM=... wins.
    if spec.tty {
        cmd.env("TERM", "xterm");
    } else {
        cmd.env_remove("TERM");
    }
    let identity = spec.user.as_deref().map(resolve_identity).transpose()?;
    if let Some(home) = identity
        .as_ref()
        .and_then(|identity| identity.home.as_ref())
    {
        cmd.env("HOME", home);
    }
    // After the identity's HOME, so an explicit -e HOME=... wins.
    for (key, value) in &spec.env {
        cmd.env(key, value);
    }
    if let Some(workdir) = &spec.workdir {
        cmd.current_dir(workdir);
    }
    Ok((cmd, identity))
}

/// Make the child switch to `identity` just before exec. Register this after
/// every other `pre_exec`: std runs them in order. The groups were resolved
/// before fork, so the child only makes system calls.
fn run_as(cmd: &mut tokio::process::Command, identity: Option<Identity>) {
    let Some(Identity {
        uid, gid, groups, ..
    }) = identity
    else {
        return;
    };
    unsafe {
        cmd.pre_exec(move || {
            if libc::setgroups(groups.len(), groups.as_ptr()) < 0
                || libc::setgid(gid) < 0
                || libc::setuid(uid) < 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

fn spawn(spec: &SessionSpec) -> std::io::Result<Spawned> {
    let (mut cmd, identity) = base_command(spec)?;

    if spec.tty {
        let (master, slave) = open_pty()?;
        if let Some(size) = spec.size {
            set_winsize(master.as_raw_fd(), size)?;
        }
        if spec.raw_pty {
            make_raw(slave.as_raw_fd())?;
        }
        cmd.stdin(Stdio::from(slave.try_clone()?));
        cmd.stdout(Stdio::from(slave.try_clone()?));
        cmd.stderr(Stdio::from(slave));
        // New session with the PTY as controlling terminal, so Ctrl-C and
        // window changes reach the command. Runs after stdio is in place.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        run_as(&mut cmd, identity);
        let child = cmd.spawn()?;
        // Command holds the parent's slave fds. Close them now, or the master
        // never reports end of output.
        drop(cmd);
        let master = AsyncFdStream::new(master)?;
        let stdin = spec.interactive.then(|| StdinSink::Pty(master.handle()));
        let pty = Some(master.handle());
        Ok(Spawned {
            child,
            outputs: vec![Output {
                source: Box::new(master),
                stream: OutputStream::Data,
                is_pty: true,
            }],
            stdin,
            pty,
        })
    } else {
        cmd.stdin(if spec.interactive {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        // Own process group, so a host disconnect can kill the command and
        // everything it started.
        cmd.process_group(0);
        run_as(&mut cmd, identity);
        let mut child = cmd.spawn()?;
        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");
        let stdin = child.stdin.take().map(StdinSink::Pipe);
        Ok(Spawned {
            child,
            outputs: vec![
                Output {
                    source: Box::new(stdout),
                    stream: OutputStream::Data,
                    is_pty: false,
                },
                Output {
                    source: Box::new(stderr),
                    stream: OutputStream::Stderr,
                    is_pty: false,
                },
            ],
            stdin,
            pty: None,
        })
    }
}

/// Text sent to the client when the command could not be started.
///
/// A failed fork is the one case a host-side diagnostic can never reach,
/// because serving it needs the same fork. Carry the fork-free vitals sample
/// for every error except the two that only describe the path.
fn spawn_error_text(program: &str, error: &std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied => {
            format!("Error: cannot run {:?}: {}\n", program, error)
        }
        _ => format!(
            "Error: cannot run {:?}: {} | guest vitals: {}\n",
            program,
            error,
            crate::vitals::sample_line()
        ),
    }
}

/// How many STDIN bytes the host may have in flight: sent, and not yet taken
/// by the command. It is also the most input fc-agent holds for one session.
const STDIN_WINDOW: u32 = 256 * 1024;

/// How long a PTY is read after the command exits. It normally hangs up at
/// once; the wait only runs out when a background process keeps it open.
const PTY_DRAIN_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

/// Forward one output stream to the host until it ends or the command exits.
///
/// With `keep_reading`, a host that stopped listening does not stop the pump:
/// output is read and dropped, so the command never blocks on it.
async fn pump(
    output: Output,
    writer: FrameWriter,
    mut exited: watch::Receiver<bool>,
    keep_reading: bool,
) {
    let Output {
        mut source,
        stream,
        is_pty,
    } = output;
    let mut buf = vec![0u8; exec_proto::IO_CHUNK];
    let mut host_listening = true;
    // Checked on every pass, so a source that is never empty cannot keep the
    // pump from noticing that the command has exited.
    while !*exited.borrow() {
        tokio::select! {
            read = source.read(&mut buf) => match read {
                // End of output. A PTY master reports EIO once every slave fd is closed.
                Ok(0) => return,
                Ok(n) => {
                    if host_listening && send(&writer, &stream.frame(&buf[..n])).await.is_err() {
                        if !keep_reading {
                            return;
                        }
                        host_listening = false;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return,
            },
            // The block drops wait_for's guard, which is not Send, before the next await.
            () = async { let _ = exited.wait_for(|exited| *exited).await; } => {}
        }
    }
    if !host_listening {
        return;
    }
    if is_pty {
        drain_pty(&mut source, stream, &writer, &mut buf).await;
    } else {
        drain(source.as_raw_fd(), stream, &writer, &mut buf).await;
    }
}

/// Forward what a pipe held when the command exited, and no more.
///
/// A pipe write lands in the pipe before `write` returns, so the byte count at
/// exit covers everything the command wrote. A background process that
/// inherited the pipe may keep writing; that is not this session's output, and
/// following it would never end. Reads go straight to the fd, because tokio's
/// readiness can lag behind bytes written just before the exit.
async fn drain(fd: RawFd, stream: OutputStream, writer: &FrameWriter, buf: &mut [u8]) {
    let mut buffered: libc::c_int = 0;
    if unsafe { libc::ioctl(fd, libc::FIONREAD as _, &mut buffered) } < 0 {
        return;
    }
    let mut remaining = buffered.max(0) as usize;
    while remaining > 0 {
        let want = remaining.min(buf.len());
        // The fd is non-blocking: tokio sets that on child pipes.
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), want) };
        if n > 0 {
            let n = n as usize;
            if send(writer, &stream.frame(&buf[..n])).await.is_err() {
                return;
            }
            remaining -= n;
            continue;
        }
        if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        return;
    }
}

/// Forward a PTY's output until it hangs up.
///
/// The kernel moves slave writes to the master in a deferred flush, so neither
/// a byte count nor an empty read proves the command's last output has
/// arrived. The hangup does: it follows the flush once every slave fd is
/// closed. A background process that holds the PTY open is cut off after
/// [`PTY_DRAIN_WAIT`].
async fn drain_pty(
    source: &mut Box<dyn Source>,
    stream: OutputStream,
    writer: &FrameWriter,
    buf: &mut [u8],
) {
    let deadline = tokio::time::Instant::now() + PTY_DRAIN_WAIT;
    loop {
        match tokio::time::timeout_at(deadline, source.read(buf)).await {
            Ok(Ok(n)) if n > 0 => {
                if send(writer, &stream.frame(&buf[..n])).await.is_err() {
                    return;
                }
            }
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::Interrupted => {}
            // Hung up (EIO or end of file), or still held open after the wait.
            _ => return,
        }
    }
}

/// What the host sends that has to wait for the command.
enum HostInput {
    Stdin(Vec<u8>),
    Eof,
}

/// Read the host's frames. A failed read means the host is gone.
///
/// Only stdin is queued for [`apply_input`], and `window` counts the bytes of
/// it the host may still send, so the queue never holds more than the window.
/// A host that sends more has broken the protocol, and the session ends as if
/// it had gone away. Everything else is dealt with here: a resize is one
/// ioctl, and it must not wait behind a command that is not reading.
async fn read_frames(
    mut conn: AsyncFdStream,
    input: tokio::sync::mpsc::UnboundedSender<HostInput>,
    window: Option<Arc<AtomicI64>>,
    pty: Option<AsyncFdStream>,
    peer_gone: Arc<Notify>,
) {
    let mut ended = false;
    while let Ok(frame) = Message::read_from_async(&mut conn).await {
        let queued = match frame {
            Message::Stdin(data) => {
                // Without -i there is no stdin to deliver to, and an empty
                // chunk delivers nothing and has nothing to grant back.
                let Some(window) = &window else { continue };
                if data.is_empty() {
                    continue;
                }
                let taken = data.len() as i64;
                if window.fetch_sub(taken, Ordering::AcqRel) < taken {
                    eprintln!("[fc-agent] exec: the host sent stdin beyond the granted window");
                    break;
                }
                input.send(HostInput::Stdin(data))
            }
            // Once is enough, and a repeat must not grow the queue.
            Message::StdinEof if ended => continue,
            Message::StdinEof => {
                ended = true;
                input.send(HostInput::Eof)
            }
            Message::Resize(size) => {
                // The kernel signals the PTY's foreground process group when
                // the size changes, which is how the command learns of it.
                if let Some(pty) = &pty {
                    if let Err(e) = set_winsize(pty.as_raw_fd(), size) {
                        eprintln!("[fc-agent] exec: cannot resize the PTY: {e}");
                    }
                }
                continue;
            }
            // Guest-to-host frame types: a host has no business sending them.
            Message::Data(_)
            | Message::Stderr(_)
            | Message::Exit(_)
            | Message::Error(_)
            | Message::StdinWindow(_) => continue,
        };
        if queued.is_err() {
            return; // the session is over
        }
    }
    // notify_one stores a permit, so the session sees this even if it is not
    // waiting yet.
    peer_gone.notify_one();
}

/// Deliver the host's stdin to the command.
///
/// Every chunk that has been dealt with, written to the command or dropped
/// because the command closed its stdin, reopens that much of the window. The
/// host therefore sends exactly as fast as the command reads.
async fn apply_input(
    mut input: tokio::sync::mpsc::UnboundedReceiver<HostInput>,
    mut stdin: Option<StdinSink>,
    window: Option<Arc<AtomicI64>>,
    writer: FrameWriter,
) {
    while let Some(item) = input.recv().await {
        match item {
            HostInput::Stdin(data) => {
                let delivered = match stdin.as_mut() {
                    Some(StdinSink::Pipe(pipe)) => write_all(pipe, &data).await,
                    Some(StdinSink::Pty(pty)) => write_all(pty, &data).await,
                    None => true,
                };
                if !delivered {
                    stdin = None; // the command closed its stdin
                }
                if let Some(window) = &window {
                    // A frame never exceeds exec-proto's 1 MiB cap.
                    let taken = data.len() as u32;
                    window.fetch_add(i64::from(taken), Ordering::AcqRel);
                    if send(&writer, &Message::StdinWindow(taken)).await.is_err() {
                        return;
                    }
                }
            }
            HostInput::Eof => {
                if matches!(stdin, Some(StdinSink::Pipe(_))) {
                    stdin = None;
                }
            }
        }
    }
}

async fn write_all<W: AsyncWrite + Unpin>(sink: &mut W, data: &[u8]) -> bool {
    sink.write_all(data).await.is_ok() && sink.flush().await.is_ok()
}

/// Queue one frame for the host. Fails once the host no longer takes frames.
async fn send(writer: &FrameWriter, message: &Message) -> std::io::Result<()> {
    writer
        .frames
        .send(message.encode())
        .await
        .map_err(|_| std::io::ErrorKind::BrokenPipe.into())
}

fn open_pty() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    let result = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    // The command gets the slave as fds 0-2 only. It must not inherit the
    // master, or the PTY would never close.
    set_cloexec(master.as_raw_fd())?;
    set_cloexec(slave.as_raw_fd())?;
    Ok((master, slave))
}

fn make_raw(pty: RawFd) -> std::io::Result<()> {
    let mut termios: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(pty, &mut termios) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    unsafe { libc::cfmakeraw(&mut termios) };
    if unsafe { libc::tcsetattr(pty, libc::TCSANOW, &termios) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn set_winsize(pty: RawFd, size: exec_proto::TtySize) -> std::io::Result<()> {
    let winsize = libc::winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    if unsafe { libc::ioctl(pty, libc::TIOCSWINSZ as _, &winsize) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn set_cloexec(fd: RawFd) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
#[path = "tty_tests.rs"]
mod tests;
