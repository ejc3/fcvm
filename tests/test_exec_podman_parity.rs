//! Differential test: `fcvm exec` must behave like `podman exec`.
//!
//! One alpine container runs under host podman and one inside a VM. Every case
//! runs through `podman exec`, `fcvm exec` and `fcvm exec --vm` (one case,
//! `--privileged`, has no `--vm` form), and the results must agree on stdout bytes, stderr bytes, exit code
//! and on whether the call returned at all.
//!
//! Known, deliberate differences are not compared and are listed at the case
//! that would show them.

#![cfg(feature = "integration-slow")]

mod common;

use anyhow::{Context, Result};
use std::io::Write;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How the client's stdin is set up.
#[derive(Clone)]
enum Input {
    /// /dev/null
    Null,
    /// A pipe: these bytes, then end of input.
    Bytes(Vec<u8>),
    /// A pipe: these bytes, and the pipe stays open for the whole case.
    HeldOpen(Vec<u8>),
    /// fd 0 closed before exec, as with `<&-`.
    Closed,
    /// /dev/zero: input without end, which backs up behind a command that
    /// does not read it.
    DevZero,
    /// A PTY of this size (rows, cols) on stdin and stdout.
    Pty(u16, u16),
}

/// Scripted interaction, run in order while the command runs.
#[derive(Clone)]
enum Step {
    /// Wait until the output so far contains these bytes.
    WaitFor(&'static [u8]),
    /// Type these bytes into the PTY.
    Type(&'static [u8]),
    /// Change the PTY's size; the kernel sends the client SIGWINCH.
    Resize(u16, u16),
    /// Signal the client process.
    SignalClient(libc::c_int),
}

/// Which parts of the outcome must match podman's.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Check {
    Stdout,
    Stderr,
    Exit,
    /// Returned before the case's timeout, or did not.
    Returned,
    /// Returned within `QUICK`.
    Quick,
}

const ALL: &[Check] = &[Check::Stdout, Check::Stderr, Check::Exit, Check::Returned];
/// podman's error text names its own runtime, so only the rest is comparable.
const NO_STDERR: &[Check] = &[Check::Stdout, Check::Exit, Check::Returned];
const QUICK: Duration = Duration::from_secs(10);
/// Stop after this many differences. A systematic break then reports itself
/// in a minute instead of spending a timeout on every remaining case.
const MAX_DIFFERENCES: usize = 8;

#[derive(Clone)]
struct Case {
    name: &'static str,
    flags: &'static [&'static str],
    command: Vec<String>,
    input: Input,
    steps: Vec<Step>,
    timeout: Duration,
    checks: &'static [Check],
    /// Flags built at run time, after `flags`.
    extra_flags: Vec<String>,
    /// RUST_LOG for the client; `None` runs it without one, as a user would.
    rust_log: Option<&'static str>,
    /// Skip `fcvm exec --vm`: the flag under test only exists for containers.
    container_only: bool,
    /// Close our end of the client's stdout once this many bytes have arrived,
    /// as `| head -1` does.
    close_stdout_after: Option<usize>,
    /// Some podman versions never return here. That is their defect, not a
    /// behaviour to match: when the reference hangs, fcvm must still return.
    reference_may_hang: bool,
}

fn case(name: &'static str, flags: &'static [&'static str], command: &[&str]) -> Case {
    Case {
        name,
        flags,
        command: command.iter().map(|arg| arg.to_string()).collect(),
        input: Input::Null,
        steps: Vec::new(),
        timeout: Duration::from_secs(20),
        checks: ALL,
        extra_flags: Vec::new(),
        rust_log: None,
        container_only: false,
        close_stdout_after: None,
        reference_may_hang: false,
    }
}

impl Case {
    fn input(mut self, input: Input) -> Self {
        self.input = input;
        self
    }
    fn pty(self) -> Self {
        self.input(Input::Pty(24, 80))
    }
    fn steps(mut self, steps: &[Step]) -> Self {
        self.steps = steps.to_vec();
        self
    }
    fn timeout(mut self, secs: u64) -> Self {
        self.timeout = Duration::from_secs(secs);
        self
    }
    fn checks(mut self, checks: &'static [Check]) -> Self {
        self.checks = checks;
        self
    }
    fn rust_log(mut self, filter: &'static str) -> Self {
        self.rust_log = Some(filter);
        self
    }
    fn with_flags(mut self, flags: Vec<String>) -> Self {
        self.extra_flags = flags;
        self
    }
    fn close_stdout_after(mut self, bytes: usize) -> Self {
        self.close_stdout_after = Some(bytes);
        self
    }
    fn container_only(mut self) -> Self {
        self.container_only = true;
        self
    }
    fn reference_may_hang(mut self) -> Self {
        self.reference_may_hang = true;
        self
    }
}

/// Prints every byte value 0..=255, in a shell both busybox and dash accept.
const ALL_BYTES: &str =
    r#"i=0; while [ $i -lt 256 ]; do printf "\\$(printf %03o $i)"; i=$((i+1)); done"#;

/// Shell function: wait up to 10 s until the terminal has the given size.
///
/// `podman exec -t` applies the client's size to the container's PTY shortly
/// after the command has started, so one `stty size` at startup can lose that
/// race on a busy host. fcvm sizes the PTY before the command starts (pinned by
/// fc-agent's `a_pty_starts_at_the_requested_size_and_follows_resizes`). This
/// comparison only needs both to arrive at the size.
const WAIT_SIZE: &str = r#"wait_size() { i=0; until [ "$(stty size 2>/dev/null)" = "$1" ] || [ $i -ge 100 ]; do sleep 0.1; i=$((i+1)); done; }"#;

/// `env_file` is a client-side file of KEY=VALUE lines for `--env-file`.
fn cases(env_file: &str) -> Vec<Case> {
    use Check::*;
    use Step::*;
    let sh = |name, flags, script: &str| case(name, flags, &["sh", "-c", script]);
    let all_bytes: Vec<u8> = (0..=255u8).collect();
    vec![
        // ---- no -i, no -t ----
        case("plain_no_trailing_newline", &[], &["printf", "abc"]),
        sh("plain_streams_separate", &[], "printf out; printf err >&2"),
        sh("plain_all_256_bytes", &[], ALL_BYTES),
        sh("plain_invalid_utf8", &[], r"printf 'a\377\376b\nnext\n'"),
        sh("plain_cr_and_crlf", &[], r"printf 'a\r\nb\rc'"),
        sh("plain_empty_lines", &[], r"printf '\n\nx\n\n'"),
        sh(
            "plain_long_line_1mib",
            &[],
            "head -c 1048576 /dev/zero | tr '\\0' x",
        ),
        sh(
            "plain_large_8mib",
            &[],
            "head -c 8388608 /dev/zero | tr '\\0' y",
        ),
        sh(
            "plain_stderr_no_newline",
            &[],
            "printf e1 >&2; printf e2 >&2",
        ),
        sh("plain_exit_7", &[], "exit 7"),
        sh("plain_exit_255", &[], "exit 255"),
        sh("plain_sigterm_self", &[], "kill -TERM $$"),
        sh("plain_sigkill_self", &[], "kill -KILL $$"),
        case(
            "plain_command_not_found",
            &[],
            &["definitely-not-a-command-xyz"],
        )
        .checks(NO_STDERR),
        case("plain_command_not_executable", &[], &["/etc/passwd"]).checks(NO_STDERR),
        case("plain_stdin_not_forwarded", &[], &["cat"])
            .input(Input::Bytes(b"must not arrive\n".to_vec())),
        sh(
            "plain_no_tty",
            &[],
            "[ -t 0 ] && echo tty0 || echo notty0; [ -t 1 ] && echo tty1 || echo notty1",
        ),
        // TERM without -t is not compared. It is whatever the container's
        // environment holds, and podman versions differ on whether a container
        // gets TERM=xterm by default. fc-agent's
        // `term_is_not_inherited_without_a_pty` pins that fcvm adds none.
        sh("plain_args_with_hyphens", &[], "echo \"$@\"").with_args(&["_", "-la", "--foo", "-x"]),
        case(
            "plain_args_with_spaces_and_quotes",
            &[],
            &["printf", "%s|", "a b", "c'd", "e\"f", ""],
        ),
        // Nobody reads the output any more, as in `... yes | head -1`. The
        // client dies of SIGPIPE, which a shell reports as 141.
        // `timeout` bounds the writer: a container process outlives its client,
        // under podman and under fcvm alike.
        case("plain_client_stdout_closed", &[], &["timeout", "20", "yes"])
            .close_stdout_after(2)
            .checks(&[Exit, Returned, Quick]),
        // The same while input is backed up behind a command that never reads
        // it. The client must not wait for a write the guest will never take.
        case(
            "i_client_stdout_closed_with_input_backed_up",
            &["-i"],
            &["timeout", "20", "yes"],
        )
        .input(Input::DevZero)
        .close_stdout_after(2)
        .checks(&[Exit, Returned, Quick]),
        // A background process keeps writing to the inherited stdout. How much
        // of that arrives is a race in podman too, so only the return is compared.
        sh(
            "plain_background_process_keeps_writing",
            &[],
            "timeout 15 yes & exit 0",
        )
        .checks(&[Exit, Returned, Quick]),
        // A background process keeps stdout open for 30 s. podman returns when
        // the command itself exits.
        sh(
            "plain_background_process_holds_stdout",
            &[],
            "sleep 30 & echo hi",
        )
        .checks(&[Stdout, Stderr, Exit, Returned, Quick]),
        // Deliberate difference, not compared: on client death fcvm kills the
        // command's process group (#636) and podman leaves it running. Only
        // the client's own prompt exit is compared.
        sh(
            "plain_client_sigterm",
            &[],
            "echo READY; i=0; while [ $i -lt 100 ]; do sleep 0.1; i=$((i+1)); done",
        )
        .steps(&[WaitFor(b"READY"), SignalClient(libc::SIGTERM)])
        .checks(&[Returned, Quick]),
        // ---- -e, --env-file, -w, -u, --privileged, -d ----
        sh(
            "env_flags",
            &["-e", "FOO=bar", "-e", "BAZ=q x", "-e", "EQ=a=b"],
            r#"echo "$FOO/$BAZ/$EQ""#,
        ),
        case(
            "env_file_then_flag_wins",
            &[],
            &["sh", "-c", r#"echo "A=$A B=$B EMPTY=[${EMPTY-unset}]""#],
        )
        .with_flags(vec![
            "--env-file".into(),
            env_file.into(),
            "-e".into(),
            "A=override".into(),
        ]),
        case("workdir", &["-w", "/tmp"], &["pwd"]),
        case(
            "workdir_missing",
            &["-w", "/definitely/not/a/dir"],
            &["pwd"],
        )
        .checks(NO_STDERR),
        sh("user_by_name", &["-u", "nobody"], "id -u; id -g; id -G"),
        sh(
            "user_and_group_by_number",
            &["-u", "65534:100"],
            "id -u; id -g; id -G",
        ),
        sh("user_unknown_number", &["-u", "12345"], "id -u; id -g"),
        sh(
            "privileged",
            &["--privileged"],
            "grep CapEff /proc/self/status",
        )
        .container_only(),
        // The identifier line differs (podman prints a session id, a guest
        // command its pid), so only the prompt, successful return is compared.
        sh("detach_returns_at_once", &["-d"], "sleep 30").checks(&[Exit, Returned, Quick]),
        sh("detach_with_tty", &["-d", "-t"], "sleep 30").checks(&[Exit, Returned, Quick]),
        // ---- -i ----
        case("i_cat_reads_to_end_of_input", &["-i"], &["cat"])
            .input(Input::Bytes(b"hello\n".to_vec())),
        case("i_cat_empty_input", &["-i"], &["cat"]).input(Input::Bytes(Vec::new())),
        case("i_cat_dev_null", &["-i"], &["cat"]),
        case("i_cat_closed_stdin", &["-i"], &["cat"])
            .input(Input::Closed)
            .reference_may_hang(),
        case("i_wc_1mib", &["-i"], &["wc", "-c"]).input(Input::Bytes(vec![b'z'; 1 << 20])),
        case("i_binary_roundtrip", &["-i"], &["cat"]).input(Input::Bytes(all_bytes.repeat(1024))),
        case("i_no_trailing_newline", &["-i"], &["cat"]).input(Input::Bytes(b"abc".to_vec())),
        sh("i_streams_separate", &["-i"], "cat; printf err >&2")
            .input(Input::Bytes(b"in\n".to_vec())),
        sh("i_exit_code", &["-i"], "cat >/dev/null; exit 9").input(Input::Bytes(b"x\n".to_vec())),
        sh("i_sigterm_self", &["-i"], "kill -TERM $$").input(Input::Bytes(Vec::new())),
        // fcvm's logging shares stderr with the command's. With debug logging on,
        // the input thread logs before it announces end of input, and that log
        // line must not be able to block it. Logs land on stderr, so only the
        // rest is compared.
        case("i_cat_with_debug_logging", &["-i"], &["cat"])
            .input(Input::Bytes(b"hello\n".to_vec()))
            .rust_log("fcvm=debug")
            .checks(NO_STDERR),
        // The command does not read for 12 s, longer than any write timeout, while
        // 4 MiB of input backs up. None of it may be dropped.
        sh("i_slow_reader_gets_all_input", &["-i"], "sleep 12; wc -c")
            .input(Input::Bytes(vec![b's'; 4 << 20]))
            .timeout(60),
        // The command exits while the client's stdin is still open.
        case(
            "i_command_exits_before_end_of_input",
            &["-i"],
            &["head", "-1"],
        )
        .input(Input::HeldOpen(b"first\nsecond\n".to_vec()))
        .checks(&[Stdout, Stderr, Exit, Returned, Quick]),
        sh(
            "i_background_process_holds_stdout",
            &["-i"],
            "sleep 30 & echo hi",
        )
        .input(Input::Bytes(Vec::new()))
        .checks(&[Stdout, Stderr, Exit, Returned, Quick]),
        case(
            "i_command_not_found",
            &["-i"],
            &["definitely-not-a-command-xyz"],
        )
        .input(Input::Bytes(Vec::new()))
        .checks(NO_STDERR),
        // ---- -t and -it, client on a PTY ----
        case("t_newline_translation", &["-t"], &["printf", "a\\nb"]).pty(),
        sh("t_stderr_merged", &["-t"], "echo out; echo err >&2").pty(),
        sh(
            "t_all_three_are_ttys",
            &["-t"],
            "[ -t 0 ] && echo tty0; [ -t 1 ] && echo tty1; [ -t 2 ] && echo tty2",
        )
        .pty(),
        sh("t_term_is_xterm", &["-t"], "echo TERM=${TERM-unset}").pty(),
        sh("t_exit_code", &["-t"], "exit 5").pty(),
        case(
            "t_explicit_term_wins",
            &["-t", "-e", "TERM=screen-256color"],
            &["printenv", "TERM"],
        )
        .pty(),
        sh(
            "t_initial_size",
            &["-t"],
            &format!("{WAIT_SIZE}; wait_size '41 123'; stty size"),
        )
        .input(Input::Pty(41, 123)),
        sh(
            "it_initial_size",
            &["-it"],
            &format!("{WAIT_SIZE}; wait_size '37 111'; stty size"),
        )
        .input(Input::Pty(37, 111)),
        // The script waits for each size itself, so the only step is the resize.
        sh(
            "it_resize",
            &["-it"],
            &format!("{WAIT_SIZE}; wait_size '24 80'; echo READY; wait_size '50 132'; stty size"),
        )
        .pty()
        .steps(&[WaitFor(b"READY"), Resize(50, 132)]),
        sh(
            "it_ctrl_d_is_end_of_input",
            &["-it"],
            "echo READY; cat; echo GOT_EOF",
        )
        .pty()
        .steps(&[
            WaitFor(b"READY"),
            Type(b"hi\n"),
            // The PTY echoes the line once it has taken it.
            WaitFor(b"hi"),
            Type(b"\x04"),
        ]),
        // The shell stays in a builtin loop after READY and forks nothing. With
        // `sleep` there, a Ctrl-C that lands between the fork and the exec is
        // taken by the forked child in the shell's own handler, the real sleep
        // then runs its full time, and how often that happens depends on how
        // fast the client delivers the key.
        sh(
            "it_ctrl_c_interrupts",
            &["-it"],
            "trap 'echo CAUGHT; exit 130' INT; echo READY; while :; do :; done",
        )
        .pty()
        .steps(&[WaitFor(b"READY"), Type(b"\x03")]),
        // Without -i the typed line never reaches the PTY, so it is not echoed.
        sh(
            "t_without_i_ignores_typing",
            &["-t"],
            "echo READY; sleep 2; echo DONE",
        )
        .pty()
        .steps(&[WaitFor(b"READY"), Type(b"typed\n")]),
        sh(
            "it_all_256_bytes",
            &["-it"],
            &format!("stty raw -echo; {ALL_BYTES}"),
        )
        .pty(),
        sh(
            "t_large_1mib",
            &["-t"],
            "head -c 1048576 /dev/zero | tr '\\0' q",
        )
        .pty(),
        // ---- -t asked for, but the client's stdin is a pipe ----
        sh(
            "t_with_piped_stdin",
            &["-t"],
            "[ -t 0 ] && echo tty0 || echo notty0; echo done",
        )
        .input(Input::Bytes(Vec::new())),
        case("it_with_piped_stdin", &["-it"], &["head", "-1"])
            .input(Input::Bytes(b"piped\n".to_vec())),
        // A PTY has no half-close, so `cat` never sees the pipe's end of input
        // and the call never returns. podman behaves the same.
        case("it_with_piped_stdin_never_ends", &["-it"], &["cat"])
            .input(Input::Bytes(b"piped\n".to_vec()))
            .timeout(8),
    ]
}

impl Case {
    fn with_args(mut self, args: &[&str]) -> Self {
        self.command.extend(args.iter().map(|arg| arg.to_string()));
        self
    }
}

#[derive(Debug)]
struct Outcome {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    /// Exit code, or 128+signal. `None` when the case timed out and was killed.
    exit: Option<i32>,
    wall: Duration,
}

/// A pipe the client does not inherit. `Stdio::from` hands the client the one
/// end it should have, as fd 0, 1 or 2. An inherited write end of its own stdin
/// pipe would keep that pipe open forever, and end of input would never arrive.
fn cloexec_pipe() -> nix::Result<(OwnedFd, OwnedFd)> {
    nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC)
}

fn set_cloexec(fd: &impl AsRawFd) {
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
    let rc = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, flags | libc::FD_CLOEXEC) };
    assert_eq!(rc, 0, "F_SETFD failed");
}

fn set_nonblocking(fd: &impl AsRawFd) {
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) };
}

fn set_winsize(fd: &impl AsRawFd, rows: u16, cols: u16) {
    let size = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCSWINSZ, &size) };
    assert_eq!(rc, 0, "TIOCSWINSZ failed");
}

/// Read what is available right now. Returns false once the fd is finished.
fn read_available(fd: &OwnedFd, into: &mut Vec<u8>) -> bool {
    let mut buf = [0u8; 65536];
    loop {
        let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        if n > 0 {
            into.extend_from_slice(&buf[..n as usize]);
            continue;
        }
        if n == 0 {
            return false;
        }
        // EAGAIN: nothing more for now. EIO: a PTY whose other side closed.
        return std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock;
    }
}

/// Run `argv` as the case describes and collect its outcome.
fn run(case: &Case, prefix: &[String]) -> Result<Outcome> {
    let mut argv: Vec<String> = prefix.to_vec();
    let mut command = Command::new(argv.remove(0));
    command.args(&argv);
    // The comparison is of what a user sees. `make test-root` exports
    // RUST_LOG=fcvm=debug, which would put fcvm's own log on stderr.
    match &case.rust_log {
        Some(filter) => command.env("RUST_LOG", filter),
        None => command.env_remove("RUST_LOG"),
    };

    let started = Instant::now();
    let (stderr_rx, stderr_tx) = cloexec_pipe()?;
    command.stderr(Stdio::from(stderr_tx));

    // `output` is the child's stdout (a pipe) or the PTY master.
    let mut stdin_pipe: Option<(OwnedFd, Vec<u8>, bool)> = None;
    let mut master: Option<OwnedFd> = None;
    let output: OwnedFd = match &case.input {
        Input::Pty(rows, cols) => {
            let pty = nix::pty::openpty(None, None)?;
            // The client gets the slave as fds 0 and 1 only, never the master.
            set_cloexec(&pty.master);
            set_cloexec(&pty.slave);
            set_winsize(&pty.slave, *rows, *cols);
            command.stdin(Stdio::from(pty.slave.try_clone()?));
            command.stdout(Stdio::from(pty.slave));
            // The PTY becomes the client's controlling terminal, so a resize
            // reaches it as SIGWINCH.
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            master = Some(pty.master.try_clone()?);
            pty.master
        }
        other => {
            let (stdout_rx, stdout_tx) = cloexec_pipe()?;
            command.stdout(Stdio::from(stdout_tx));
            match other {
                Input::Null => {
                    command.stdin(Stdio::null());
                }
                Input::DevZero => {
                    command.stdin(Stdio::from(std::fs::File::open("/dev/zero")?));
                }
                Input::Bytes(bytes) | Input::HeldOpen(bytes) => {
                    let (rx, tx) = cloexec_pipe()?;
                    command.stdin(Stdio::from(rx));
                    set_nonblocking(&tx);
                    let hold = matches!(other, Input::HeldOpen(_));
                    stdin_pipe = Some((tx, bytes.clone(), hold));
                }
                Input::Closed => {
                    command.stdin(Stdio::null());
                    unsafe {
                        command.pre_exec(|| {
                            libc::close(0);
                            Ok(())
                        });
                    }
                }
                Input::Pty(..) => unreachable!(),
            }
            stdout_rx
        }
    };

    let mut child = command
        .spawn()
        .with_context(|| format!("spawning {:?}", prefix[0]))?;
    // Drop the parent's copies of the child's fds, or the pipes never end.
    drop(command);
    set_nonblocking(&output);
    set_nonblocking(&stderr_rx);

    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let mut output = Some(output);
    let mut stderr_open = true;
    let mut steps = case.steps.iter().peekable();
    let mut exited_at: Option<Instant> = None;
    let deadline = started + case.timeout;
    let mut exit = None;

    loop {
        let now = Instant::now();
        if now > deadline {
            let _ = child.kill();
            let _ = child.wait();
            break;
        }

        if let Some((tx, pending, hold)) = stdin_pipe.as_mut() {
            if !pending.is_empty() {
                let n =
                    unsafe { libc::write(tx.as_raw_fd(), pending.as_ptr().cast(), pending.len()) };
                if n > 0 {
                    pending.drain(..n as usize);
                } else if n < 0
                    && std::io::Error::last_os_error().kind() != std::io::ErrorKind::WouldBlock
                {
                    pending.clear(); // the command closed its stdin
                }
            }
            if pending.is_empty() && !*hold {
                stdin_pipe = None; // closes the pipe: end of input
            }
        }

        while let Some(step) = steps.peek() {
            match step {
                Step::WaitFor(needle) => {
                    if !stdout.windows(needle.len()).any(|window| window == *needle) {
                        break;
                    }
                }
                Step::Type(bytes) => {
                    let mut master = std::fs::File::from(
                        master.as_ref().context("Type needs a PTY")?.try_clone()?,
                    );
                    master.write_all(bytes)?;
                }
                Step::Resize(rows, cols) => {
                    set_winsize(master.as_ref().context("Resize needs a PTY")?, *rows, *cols)
                }
                Step::SignalClient(signal) => {
                    unsafe { libc::kill(child.id() as i32, *signal) };
                }
            }
            steps.next();
        }

        // An fd of -1 makes poll skip the entry.
        let mut fds = [
            output.as_ref().map_or(-1, |fd| fd.as_raw_fd()),
            stderr_rx.as_raw_fd(),
        ]
        .map(|fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        });
        unsafe { libc::poll(fds.as_mut_ptr(), 2, 50) };
        if let Some(fd) = &output {
            let open = read_available(fd, &mut stdout);
            let enough = case
                .close_stdout_after
                .is_some_and(|bytes| stdout.len() >= bytes);
            if !open || enough {
                output = None; // closes our end
            }
        }
        if stderr_open {
            stderr_open = read_available(&stderr_rx, &mut stderr);
        }

        if exit.is_none() {
            if let Some(status) = child.try_wait()? {
                use std::os::unix::process::ExitStatusExt;
                exit = Some(
                    status
                        .code()
                        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)),
                );
                exited_at = Some(Instant::now());
            }
        }
        // After the client exits, give its output one second to arrive. A PTY
        // master may never report end of file, so do not wait for that.
        if let Some(at) = exited_at {
            if (output.is_none() && !stderr_open) || at.elapsed() > Duration::from_secs(1) {
                break;
            }
        }
    }

    Ok(Outcome {
        stdout,
        stderr,
        exit,
        wall: started.elapsed(),
    })
}

fn show(bytes: &[u8]) -> String {
    let head = &bytes[..bytes.len().min(120)];
    format!(
        "[{} bytes] {:?}",
        bytes.len(),
        String::from_utf8_lossy(head)
    )
}

/// Compare one target's outcome with podman's. Returns the mismatches.
fn differences(case: &Case, podman: &Outcome, fcvm: &Outcome) -> Vec<String> {
    if case.reference_may_hang && podman.exit.is_none() {
        return match fcvm.exit {
            Some(_) => Vec::new(),
            None => vec!["returned: fcvm false, and the reference hanging is no reason to".into()],
        };
    }
    let mut found = Vec::new();
    for check in case.checks {
        let (same, what) = match check {
            Check::Stdout => (
                podman.stdout == fcvm.stdout,
                format!(
                    "stdout: podman {} fcvm {}",
                    show(&podman.stdout),
                    show(&fcvm.stdout)
                ),
            ),
            Check::Stderr => (
                podman.stderr == fcvm.stderr,
                format!(
                    "stderr: podman {} fcvm {}",
                    show(&podman.stderr),
                    show(&fcvm.stderr)
                ),
            ),
            Check::Exit => (
                podman.exit == fcvm.exit,
                format!("exit: podman {:?} fcvm {:?}", podman.exit, fcvm.exit),
            ),
            Check::Returned => (
                podman.exit.is_some() == fcvm.exit.is_some(),
                format!(
                    "returned: podman {} fcvm {}",
                    podman.exit.is_some(),
                    fcvm.exit.is_some()
                ),
            ),
            Check::Quick => (
                (podman.wall < QUICK) == (fcvm.wall < QUICK),
                format!("took: podman {:?} fcvm {:?}", podman.wall, fcvm.wall),
            ),
        };
        if !same {
            found.push(what);
        }
    }
    found
}

/// The reference container's own lifetime. Longer than nextest allows this
/// test, so it never ends mid-run.
const REFERENCE_LIFETIME_SECS: &str = "1800";

/// Removes the reference container as soon as the test ends.
///
/// A killed test never runs this. The container is therefore started with
/// `--rm` and a bounded `sleep`, so it removes itself with no help from here.
struct ReferenceContainer(String);

impl Drop for ReferenceContainer {
    fn drop(&mut self) {
        let _ = Command::new("podman")
            .args(["rm", "-f", "-t", "0", &self.0])
            .output();
    }
}

fn start_reference_container(name: &str) -> Result<ReferenceContainer> {
    // The guard exists before `podman run`: a start that fails half way can
    // leave a created container behind.
    let reference = ReferenceContainer(name.to_string());
    // No network: no case needs one, and a host without working container
    // networking can still run the comparison.
    let output = Command::new("podman")
        .args([
            "run",
            "-d",
            "--rm",
            "--name",
            name,
            "--network",
            "none",
            common::ALPINE_IMAGE,
            "sleep",
            REFERENCE_LIFETIME_SECS,
        ])
        .output()
        .context("running host podman")?;
    anyhow::ensure!(
        output.status.success(),
        "host podman could not start the reference container: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(reference)
}

#[tokio::test(flavor = "multi_thread")]
async fn test_exec_matches_podman_exec() -> Result<()> {
    let fcvm_path: PathBuf = common::find_fcvm_binary()?;
    let (vm_name, reference_name, _, _) = common::unique_names("exec-parity");

    let (mut _child, fcvm_pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        common::ALPINE_IMAGE,
        "sleep",
        "infinity",
    ])
    .await
    .context("spawning fcvm podman run")?;
    if let Err(e) = common::poll_health_by_pid(fcvm_pid, 300).await {
        common::kill_process(fcvm_pid).await;
        return Err(e.context("VM failed to become healthy"));
    }

    // The cases are blocking process I/O. Keep them off the runtime threads,
    // which drain the VM's log pipes.
    let result = tokio::task::spawn_blocking(move || -> Result<Vec<String>> {
        let reference = start_reference_container(&reference_name)?;
        let podman = vec!["podman".to_string(), "exec".to_string()];
        let fcvm = |vm: bool| {
            let mut prefix = vec![
                fcvm_path.to_string_lossy().into_owned(),
                "exec".to_string(),
                "--pid".to_string(),
                fcvm_pid.to_string(),
            ];
            if vm {
                prefix.push("--vm".to_string());
            }
            prefix
        };

        let mut failures = Vec::new();
        let env_file = tempfile::NamedTempFile::new().context("creating the env file")?;
        std::fs::write(
            env_file.path(),
            "A=1\n# a comment\n\nB=two words\nEMPTY=\n",
        )?;
        let cases = cases(&env_file.path().to_string_lossy());
        let total = cases.len();
        let mut ran = 0usize;
        for case in cases {
            ran += 1;
            let target = |mut prefix: Vec<String>, separator: &[&str]| {
                prefix.extend(case.flags.iter().map(|flag| flag.to_string()));
                prefix.extend(case.extra_flags.iter().cloned());
                prefix.extend(separator.iter().map(|part| part.to_string()));
                prefix.extend(case.command.iter().cloned());
                prefix
            };
            let mut expected = run(&case, &target(podman.clone(), &[&reference.0]))?;
            // podman's own failures are sometimes transient under load (seen: a
            // user lookup in the image failing once). One that is real repeats.
            if expected.stderr.starts_with(b"Error:") {
                println!("  {:44} podman reported {}; asking again", case.name, show(&expected.stderr));
                expected = run(&case, &target(podman.clone(), &[&reference.0]))?;
            }
            let report = |label: &str, outcome: &Outcome| {
                println!(
                    "  {:44} {:16} exit {:?} in {:.2?}",
                    case.name, label, outcome.exit, outcome.wall
                );
            };
            report("podman exec", &expected);
            for (label, vm) in [("fcvm exec", false), ("fcvm exec --vm", true)] {
                if vm && case.container_only {
                    continue;
                }
                let actual = run(&case, &target(fcvm(vm), &["--"]))?;
                report(label, &actual);
                for difference in differences(&case, &expected, &actual) {
                    failures.push(format!("{} [{label}] {difference}", case.name));
                }
            }
            if failures.len() >= MAX_DIFFERENCES {
                break;
            }
        }
        // A detached command really runs: it leaves a marker that a later exec sees.
        // With -t it has a terminal of its own. TERM is not compared: whether a
        // container's environment holds one differs between podman versions.
        // fc-agent's `a_detached_tty_command_has_a_terminal_and_outlives_the_session`
        // pins that fcvm adds none, as podman 5.8 does.
        let mut detached_terminals = Vec::new();
        for (label, prefix, separator) in [
            ("podman exec", podman.clone(), reference.0.clone()),
            ("fcvm exec", fcvm(false), "--".to_string()),
            ("fcvm exec --vm", fcvm(true), "--".to_string()),
        ] {
            let marker = format!("/tmp/parity-detached-{}", label.replace(' ', "-"));
            let run_with = |flags: &[&str], command: &[&str]| {
                let mut argv = prefix.clone();
                argv.extend(flags.iter().map(|flag| flag.to_string()));
                argv.push(separator.clone());
                argv.extend(command.iter().map(|part| part.to_string()));
                Command::new(&argv[0])
                    .args(&argv[1..])
                    .env_remove("RUST_LOG")
                    .stdin(Stdio::null())
                    .output()
            };
            let started = run_with(&["-d"], &["sh", "-c", &format!("touch {marker}; sleep 30")])?;
            let deadline = Instant::now() + Duration::from_secs(20);
            let seen = loop {
                if run_with(&[], &["test", "-f", &marker])?.status.success() {
                    break true;
                }
                if Instant::now() > deadline {
                    break false;
                }
                std::thread::sleep(Duration::from_millis(200));
            };
            let identifier_lines = started.stdout.iter().filter(|byte| **byte == b'\n').count();
            if !started.status.success() || identifier_lines != 1 || !seen {
                failures.push(format!(
                    "detached_command_runs [{label}] started ok: {}, identifier lines: {identifier_lines}, marker seen: {seen}",
                    started.status.success()
                ));
            }

            let report = format!("{marker}-tty");
            let script = format!(
                "exec 3>{report}.part; tty >&3; echo unread; \
                 mv {report}.part {report}; sleep 30"
            );
            run_with(&["-d", "-t"], &["sh", "-c", &script])?;
            let deadline = Instant::now() + Duration::from_secs(20);
            let text = loop {
                let shown = run_with(&[], &["cat", &report])?;
                if shown.status.success() || Instant::now() > deadline {
                    break String::from_utf8_lossy(&shown.stdout).into_owned();
                }
                std::thread::sleep(Duration::from_millis(200));
            };
            // Which pts it is differs; that it is one does not.
            let text = match text.strip_prefix("/dev/pts/") {
                Some(rest) => format!("/dev/pts/N{}", rest.trim_start_matches(|c: char| c.is_ascii_digit())),
                None => text,
            };
            detached_terminals.push((label, text));
        }
        let expected = detached_terminals[0].1.clone();
        if !expected.starts_with("/dev/pts/N") {
            failures.push(format!("detached_tty_command [podman exec] reported {expected:?}, expected a terminal"));
        }
        for (label, text) in &detached_terminals[1..] {
            if *text != expected {
                failures.push(format!("detached_tty_command [{label}] podman {expected:?} fcvm {text:?}"));
            }
        }

        // The tool's own failure, as opposed to the command's: both use 125.
        let own_failure = |argv: &[&str]| -> Result<Option<i32>> {
            let status = Command::new(argv[0])
                .args(&argv[1..])
                .env_remove("RUST_LOG")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()?;
            Ok(status.code())
        };
        let podman_code = own_failure(&["podman", "exec", "no-such-container-fcvm-parity", "true"])?;
        let fcvm_binary = fcvm(false)[0].clone();
        let fcvm_code = own_failure(&[&fcvm_binary, "exec", "--name", "no-such-vm-fcvm-parity", "--", "true"])?;
        if podman_code != Some(125) || fcvm_code != podman_code {
            failures.push(format!(
                "own_failure_exit_code: podman {podman_code:?} fcvm {fcvm_code:?}, expected 125 from both"
            ));
        }

        // The count is of differences only; the notice below is not one.
        if ran == total {
            println!("{total} cases, {} differences", failures.len());
        } else {
            println!(
                "{ran} of {total} cases, {} differences, stopped at the limit",
                failures.len()
            );
            failures.push(format!(
                "stopped at {MAX_DIFFERENCES} differences after {ran} of {total} cases; later cases did not run"
            ));
        }
        Ok(failures)
    })
    .await
    .context("case runner panicked");

    common::kill_process(fcvm_pid).await;
    let failures = result??;
    assert!(
        failures.is_empty(),
        "fcvm exec differs from podman exec:\n{}",
        failures.join("\n")
    );
    Ok(())
}
