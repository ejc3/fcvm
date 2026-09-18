//! Session tests. Each runs a real command through `run_session` over a socket
//! pair and plays the host side of the framed protocol.

use super::*;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

struct Outcome {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit: Option<i32>,
    elapsed: Duration,
}

fn spec(argv: &[&str], tty: bool, interactive: bool) -> SessionSpec {
    SessionSpec {
        argv: argv.iter().map(|arg| arg.to_string()).collect(),
        env: Vec::new(),
        tty,
        interactive,
        size: None,
        raw_pty: false,
        workdir: None,
        user: None,
        detach: false,
        kill_on_disconnect: true,
    }
}

fn sh(script: &str, tty: bool, interactive: bool) -> SessionSpec {
    spec(&["sh", "-c", script], tty, interactive)
}

/// Start a session and return the host end of its connection.
fn start(spec: SessionSpec) -> (AsyncFdStream, tokio::task::JoinHandle<i32>) {
    let (host, guest) = UnixStream::pair().expect("socket pair");
    let session = tokio::spawn(run_session(OwnedFd::from(guest), spec));
    let host = AsyncFdStream::new(OwnedFd::from(host)).expect("register host end");
    (host, session)
}

/// Read frames until Exit or until the connection closes.
async fn collect(host: &mut AsyncFdStream, outcome: &mut Outcome) {
    loop {
        match Message::read_from_async(host).await {
            Ok(Message::Data(data)) => outcome.stdout.extend_from_slice(&data),
            Ok(Message::Stderr(data)) => outcome.stderr.extend_from_slice(&data),
            Ok(Message::Exit(code)) => {
                outcome.exit = Some(code);
                return;
            }
            // Flow control for forwarded stdin; these tests send little of it.
            Ok(Message::StdinWindow(_)) => {}
            Ok(other) => panic!("unexpected frame from the guest: {:?}", other),
            Err(_) => return,
        }
    }
}

/// Run `spec`, send `frames` as the host, and collect everything up to Exit.
async fn run(spec: SessionSpec, frames: Vec<Message>) -> Outcome {
    let started = Instant::now();
    let (mut host, session) = start(spec);
    let mut outcome = Outcome {
        stdout: Vec::new(),
        stderr: Vec::new(),
        exit: None,
        elapsed: Duration::ZERO,
    };
    tokio::time::timeout(Duration::from_secs(60), async {
        for frame in frames {
            host.write_all(&frame.encode()).await.expect("send frame");
        }
        collect(&mut host, &mut outcome).await;
    })
    .await
    .expect("session did not finish within 60s");
    outcome.elapsed = started.elapsed();
    let returned = session.await.expect("session task");
    assert_eq!(
        outcome.exit,
        Some(returned),
        "Exit frame and return value disagree"
    );
    outcome
}

#[tokio::test(flavor = "multi_thread")]
async fn stdout_is_byte_exact() {
    // Invalid UTF-8, CRLF, and no trailing newline all survive.
    let outcome = run(sh(r"printf 'a\377\376b\r\nc'", false, false), vec![]).await;
    assert_eq!(outcome.stdout, b"a\xff\xfeb\r\nc");
    assert_eq!(outcome.stderr, b"");
    assert_eq!(outcome.exit, Some(0));
}

#[tokio::test(flavor = "multi_thread")]
async fn stderr_is_a_separate_stream_without_a_pty() {
    for interactive in [false, true] {
        let outcome = run(
            sh("printf out; printf err >&2", false, interactive),
            vec![Message::StdinEof],
        )
        .await;
        assert_eq!(outcome.stdout, b"out", "interactive={interactive}");
        assert_eq!(outcome.stderr, b"err", "interactive={interactive}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn end_of_input_closes_the_commands_stdin() {
    let outcome = run(
        spec(&["cat"], false, true),
        vec![Message::Stdin(b"hello\n".to_vec()), Message::StdinEof],
    )
    .await;
    assert_eq!(outcome.stdout, b"hello\n");
    assert_eq!(outcome.exit, Some(0));
}

#[tokio::test(flavor = "multi_thread")]
async fn without_interactive_stdin_is_dev_null() {
    // cat sees end of input at once, and forwarded bytes go nowhere.
    let outcome = run(
        spec(&["cat"], false, false),
        vec![Message::Stdin(b"must not arrive\n".to_vec())],
    )
    .await;
    assert_eq!(outcome.stdout, b"");
    assert_eq!(outcome.exit, Some(0));
}

#[tokio::test(flavor = "multi_thread")]
async fn death_by_signal_exits_128_plus_the_signal() {
    let outcome = run(sh("kill -TERM $$", false, false), vec![]).await;
    assert_eq!(outcome.exit, Some(128 + libc::SIGTERM));
    let outcome = run(sh("kill -KILL $$", false, false), vec![]).await;
    assert_eq!(outcome.exit, Some(128 + libc::SIGKILL));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_command_exits_127_and_an_unrunnable_one_126() {
    let outcome = run(spec(&["/definitely/not/a/command"], false, false), vec![]).await;
    assert_eq!(outcome.exit, Some(127));
    assert_eq!(outcome.stdout, b"");
    let text = String::from_utf8_lossy(&outcome.stderr).into_owned();
    assert!(text.contains("/definitely/not/a/command"), "{text:?}");
    assert!(!text.contains("guest vitals"), "{text:?}");

    let outcome = run(spec(&["/etc/passwd"], false, false), vec![]).await;
    assert_eq!(outcome.exit, Some(126));
}

#[tokio::test(flavor = "multi_thread")]
async fn other_spawn_failures_carry_guest_vitals() {
    // A NUL in the program name fails before fork with neither "not found" nor
    // "permission denied": the class where the host cannot ask the guest why.
    let outcome = run(spec(&["bad\0name"], false, false), vec![]).await;
    assert_eq!(outcome.exit, Some(126));
    let text = String::from_utf8_lossy(&outcome.stderr).into_owned();
    assert!(text.contains("guest vitals: "), "{text:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_session_ends_when_the_command_exits_not_when_its_output_closes() {
    // The background sleep inherits stdout and keeps it open for 20s.
    let outcome = run(sh("sleep 20 & echo hi", false, false), vec![]).await;
    assert_eq!(outcome.stdout, b"hi\n");
    assert_eq!(outcome.exit, Some(0));
    assert!(
        outcome.elapsed < Duration::from_secs(5),
        "waited {:?} for a background process",
        outcome.elapsed
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn bytes_written_just_before_exit_are_never_lost() {
    for round in 0..200 {
        let outcome = run(sh("printf tail", false, false), vec![]).await;
        assert_eq!(outcome.stdout, b"tail", "round {round}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn large_output_arrives_complete() {
    let outcome = run(sh("head -c 8388608 /dev/zero", false, false), vec![]).await;
    assert_eq!(outcome.stdout.len(), 8 * 1024 * 1024);
    assert!(outcome.stdout.iter().all(|byte| *byte == 0));
    assert_eq!(outcome.exit, Some(0));
}

#[tokio::test(flavor = "multi_thread")]
async fn term_is_not_inherited_without_a_pty() {
    // Read the environment with `env`, not through a shell: bash sets
    // TERM=dumb for itself when it starts without one.
    std::env::set_var("TERM", "vt220");
    let outcome = run(spec(&["env"], false, false), vec![]).await;
    let text = String::from_utf8_lossy(&outcome.stdout).into_owned();
    assert!(
        text.contains("PATH="),
        "env printed nothing useful: {text:?}"
    );
    assert!(
        !text.lines().any(|line| line.starts_with("TERM=")),
        "{text:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pty_gives_the_command_a_terminal() {
    let outcome = run(
        sh(
            "[ -t 0 ] && [ -t 1 ] && [ -t 2 ] && echo all-tty; echo $TERM; echo err >&2",
            true,
            false,
        ),
        vec![],
    )
    .await;
    // The PTY merges stderr into the one stream and translates newlines.
    assert_eq!(outcome.stdout, b"all-tty\r\nxterm\r\nerr\r\n");
    assert_eq!(outcome.stderr, b"");
    assert_eq!(outcome.exit, Some(0));
}

#[tokio::test(flavor = "multi_thread")]
async fn end_of_input_leaves_a_pty_open() {
    // A PTY has no half-close. Ctrl-D sent after StdinEof must still arrive.
    let outcome = run(
        sh("stty -echo; echo READY; cat; echo GOT_EOF", true, true),
        vec![
            Message::StdinEof,
            Message::Stdin(b"typed\n".to_vec()),
            Message::Stdin(vec![0x04]),
        ],
    )
    .await;
    let text = String::from_utf8_lossy(&outcome.stdout).into_owned();
    assert!(text.contains("typed"), "{text:?}");
    assert!(text.contains("GOT_EOF"), "{text:?}");
    assert_eq!(outcome.exit, Some(0));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_disconnect_kills_the_whole_process_group() {
    for tty in [false, true] {
        // The shell prints its pid (also its process group), then waits on a
        // background child in the same group.
        let (mut host, session) = start(sh("echo $$; sleep 60 & wait", tty, false));
        let mut seen = Vec::new();
        let pid: i32 = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                match Message::read_from_async(&mut host).await.expect("frame") {
                    Message::Data(data) => seen.extend_from_slice(&data),
                    other => panic!("unexpected frame: {:?}", other),
                }
                if let Some(line) = String::from_utf8_lossy(&seen).lines().next() {
                    if seen.contains(&b'\n') {
                        return line.trim().parse().expect("pid line");
                    }
                }
            }
        })
        .await
        .expect("no pid within 30s");
        assert_eq!(
            unsafe { libc::kill(-pid, 0) },
            0,
            "group {pid} should be running"
        );

        drop(host);
        tokio::time::timeout(Duration::from_secs(10), session)
            .await
            .expect("session did not end after the host disconnected")
            .expect("session task");

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let alive = unsafe { libc::kill(-pid, 0) } == 0;
            if !alive {
                break;
            }
            assert!(Instant::now() < deadline, "tty={tty}: group {pid} survived");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

/// Read PTY output until it contains `needle`; returns everything read so far.
async fn read_until(host: &mut AsyncFdStream, seen: &mut Vec<u8>, needle: &str) {
    tokio::time::timeout(Duration::from_secs(30), async {
        while !String::from_utf8_lossy(seen).contains(needle) {
            match Message::read_from_async(host).await.expect("frame") {
                Message::Data(data) => seen.extend_from_slice(&data),
                // Flow control for forwarded stdin, not output.
                Message::StdinWindow(_) => {}
                other => panic!("unexpected frame while waiting for {needle:?}: {other:?}"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "never saw {needle:?}; got {:?}",
            String::from_utf8_lossy(seen)
        )
    });
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pty_starts_at_the_requested_size_and_follows_resizes() {
    let mut session = sh("stty size; echo READY; read line; stty size", true, true);
    session.size = Some(exec_proto::TtySize {
        rows: 41,
        cols: 123,
    });
    let (mut host, session) = start(session);

    // The first `stty size` runs before any frame could arrive, so it proves
    // the size was in place when the command started.
    let mut seen = Vec::new();
    read_until(&mut host, &mut seen, "READY").await;
    assert!(
        String::from_utf8_lossy(&seen).starts_with("41 123\r\n"),
        "{:?}",
        String::from_utf8_lossy(&seen)
    );

    let resize = Message::Resize(exec_proto::TtySize {
        rows: 50,
        cols: 132,
    });
    host.write_all(&resize.encode()).await.unwrap();
    host.write_all(&Message::Stdin(b"\n".to_vec()).encode())
        .await
        .unwrap();
    read_until(&mut host, &mut seen, "50 132").await;

    let mut outcome = Outcome {
        stdout: Vec::new(),
        stderr: Vec::new(),
        exit: None,
        elapsed: Duration::ZERO,
    };
    collect(&mut host, &mut outcome).await;
    assert_eq!(outcome.exit, Some(0));
    assert_eq!(session.await.unwrap(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_resize_without_a_pty_is_ignored() {
    let outcome = run(
        sh("echo fine", false, false),
        vec![Message::Resize(exec_proto::TtySize { rows: 9, cols: 9 })],
    )
    .await;
    assert_eq!(outcome.stdout, b"fine\n");
    assert_eq!(outcome.exit, Some(0));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_raw_pty_carries_input_without_echo_or_translation() {
    // What a podman client needs from this PTY: a terminal that passes bytes
    // through. Cooked mode would echo the line and turn its \n into \r\n.
    let mut session = sh("[ -t 0 ] && printf tty:; head -c 3", true, true);
    session.raw_pty = true;
    let outcome = run(session, vec![Message::Stdin(b"a\rb".to_vec())]).await;
    assert_eq!(outcome.stdout, b"tty:a\rb");
    assert_eq!(outcome.exit, Some(0));

    let cooked = run(
        sh("head -c 2", true, true),
        vec![Message::Stdin(b"a\n".to_vec())],
    )
    .await;
    assert_eq!(
        cooked.stdout, b"a\r\na\r\n",
        "control: a cooked PTY echoes and translates"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_request_environment_reaches_the_command() {
    let mut session = sh(r#"echo "$FOO/$BAZ""#, false, false);
    session.env = vec![
        ("FOO".to_string(), "first".to_string()),
        ("BAZ".to_string(), "q x".to_string()),
        ("FOO".to_string(), "bar".to_string()),
    ];
    let outcome = run(session, vec![]).await;
    assert_eq!(outcome.stdout, b"bar/q x\n", "the later FOO wins");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_working_directory_is_applied_and_a_missing_one_exits_127() {
    let mut session = spec(&["pwd"], false, false);
    session.workdir = Some("/tmp".to_string());
    let outcome = run(session, vec![]).await;
    assert_eq!(outcome.stdout, b"/tmp\n");
    assert_eq!(outcome.exit, Some(0));

    // `podman exec -w /missing` exits 127 as well.
    let mut session = spec(&["pwd"], false, false);
    session.workdir = Some("/definitely/not/a/dir".to_string());
    let outcome = run(session, vec![]).await;
    assert_eq!(outcome.stdout, b"");
    assert_eq!(outcome.exit, Some(127));
}

#[test]
fn identities_resolve_the_way_podman_exec_resolves_them() {
    // A known name brings its own group, and that group is among its groups.
    let nobody = resolve_identity("nobody").unwrap();
    assert_ne!(nobody.uid, 0);
    assert!(nobody.groups.contains(&nobody.gid), "{:?}", nobody.groups);
    assert!(nobody.home.is_some());

    // The same user by number.
    let by_number = resolve_identity(&nobody.uid.to_string()).unwrap();
    assert_eq!((by_number.uid, by_number.gid), (nobody.uid, nobody.gid));

    // An explicit group replaces the supplementary groups.
    let explicit = resolve_identity(&format!("{}:4242", nobody.uid)).unwrap();
    assert_eq!(
        (explicit.gid, explicit.groups.as_slice()),
        (4242, &[4242][..])
    );

    // A number with no passwd entry runs with gid 0 and no home.
    let unknown = resolve_identity("3999999").unwrap();
    assert_eq!((unknown.uid, unknown.gid), (3999999, 0));
    assert_eq!(unknown.groups, [0]);
    assert!(unknown.home.is_none());

    // An unknown name cannot be run as anything.
    let error = resolve_identity("definitely-no-such-user")
        .err()
        .expect("must fail");
    assert!(
        error.to_string().contains("definitely-no-such-user"),
        "{error}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_user_exits_126_without_running_the_command() {
    let mut session = sh("echo must-not-run", false, false);
    session.user = Some("definitely-no-such-user".to_string());
    let outcome = run(session, vec![]).await;
    assert_eq!(outcome.stdout, b"");
    assert_eq!(outcome.exit, Some(126));
    let text = String::from_utf8_lossy(&outcome.stderr).into_owned();
    assert!(text.contains("definitely-no-such-user"), "{text:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_detached_tty_command_has_a_terminal_and_outlives_the_session() {
    // What `podman exec -d -t` gives a command: a terminal on its stdio, no
    // TERM, and a life that goes on after the client has returned.
    let report = std::env::temp_dir().join(format!("fc-agent-detached-tty-{}", std::process::id()));
    let _ = std::fs::remove_file(&report);
    let script = format!(
        "exec 3>{path}; tty >&3; echo TERM in the environment: $(env | grep -c '^TERM=') >&3; \
         echo written to the terminal nobody reads; sleep 1; echo late >&3; sleep 60",
        path = report.display()
    );
    let mut session = sh(&script, true, false);
    session.detach = true;
    let outcome = run(session, vec![]).await;
    assert_eq!(outcome.exit, Some(0));
    let pid: i32 = String::from_utf8_lossy(&outcome.stdout)
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("expected a pid line, got {:?}", outcome.stdout));

    let deadline = Instant::now() + Duration::from_secs(10);
    let text = loop {
        let text = std::fs::read_to_string(&report).unwrap_or_default();
        if text.contains("late") || Instant::now() > deadline {
            break text;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    unsafe { libc::kill(pid, libc::SIGKILL) };
    let _ = std::fs::remove_file(&report);

    let lines: Vec<&str> = text.lines().collect();
    assert!(
        lines
            .first()
            .is_some_and(|line| line.starts_with("/dev/pts/")),
        "the command had no terminal: {text:?}"
    );
    // Counted in the environment: some shells invent a TERM variable of their own.
    assert_eq!(
        &lines[1..],
        ["TERM in the environment: 0", "late"],
        "{text:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_detached_command_outlives_the_session_in_its_own_session() {
    let mut session = spec(&["sleep", "60"], false, false);
    session.detach = true;
    let outcome = run(session, vec![]).await;
    assert_eq!(outcome.exit, Some(0));
    assert!(
        outcome.elapsed < Duration::from_secs(5),
        "{:?}",
        outcome.elapsed
    );

    // One identifier line: the command's pid.
    let pid: i32 = String::from_utf8_lossy(&outcome.stdout)
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("expected a pid line, got {:?}", outcome.stdout));
    // The session is over and the connection closed, yet the command runs on,
    // as the leader of a session of its own.
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        0,
        "detached command {pid} is gone"
    );
    assert_eq!(unsafe { libc::getsid(pid) }, pid);
    unsafe { libc::kill(pid, libc::SIGKILL) };
}

/// Read the first line the command prints (its pid) from the PTY or stdout.
async fn read_pid(host: &mut AsyncFdStream) -> i32 {
    let mut seen = Vec::new();
    read_until(host, &mut seen, "\n").await;
    String::from_utf8_lossy(&seen)
        .lines()
        .next()
        .and_then(|line| line.trim().parse().ok())
        .unwrap_or_else(|| {
            panic!(
                "expected a pid line, got {:?}",
                String::from_utf8_lossy(&seen)
            )
        })
}

#[tokio::test(flavor = "multi_thread")]
async fn the_console_session_survives_a_host_disconnect() {
    // `podman run -it` rides this session. A snapshot resets vsock and drops
    // the connection; the container must run on and its exit code be kept.
    for tty in [false, true] {
        let mut session = sh("echo $$; sleep 2; exit 7", tty, false);
        session.kill_on_disconnect = false;
        let started = Instant::now();
        let (mut host, session) = start(session);
        let pid = read_pid(&mut host).await;
        drop(host);

        let code = tokio::time::timeout(Duration::from_secs(20), session)
            .await
            .expect("session did not end after the command exited")
            .expect("session task");
        assert_eq!(
            code, 7,
            "tty={tty}: the command (pid {pid}) was not left to finish"
        );
        assert!(
            started.elapsed() >= Duration::from_millis(1500),
            "{:?}",
            started.elapsed()
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_background_process_that_keeps_writing_does_not_hold_the_session() {
    // `yes` inherits the output and never stops writing. `podman exec` returns
    // when the command exits and drops what the holder writes afterwards.
    for tty in [false, true] {
        let (mut host, session) = start(sh("timeout 12 yes & exit 0", tty, false));
        let mut outcome = Outcome {
            stdout: Vec::new(),
            stderr: Vec::new(),
            exit: None,
            elapsed: Duration::ZERO,
        };
        let finished =
            tokio::time::timeout(Duration::from_secs(6), collect(&mut host, &mut outcome)).await;
        assert!(
            finished.is_ok(),
            "tty={tty}: no Exit frame within 6s of the command exiting"
        );
        assert_eq!(outcome.exit, Some(0), "tty={tty}");
        assert_eq!(session.await.unwrap(), 0);
    }
}

/// Wait until the process group is gone.
async fn assert_group_dies(pid: i32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while unsafe { libc::kill(-pid, 0) } == 0 {
        assert!(Instant::now() < deadline, "process group {pid} survived");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_that_ignores_stdin_stops_the_host_at_the_window() {
    // The command never reads. The host may send one window of input and gets
    // no further grant, so the connection itself never backs up. That is what
    // lets the disconnect below be seen: over vsock a hangup does not cross a
    // backed-up connection (#636).
    let (mut host, session) = start(sh("echo $$; exec sleep 60", false, true));
    let (mut seen, mut first_grant) = (Vec::new(), None);
    while first_grant.is_none() || !seen.contains(&b'\n') {
        match Message::read_from_async(&mut host).await.expect("frame") {
            Message::Data(data) => seen.extend_from_slice(&data),
            Message::StdinWindow(bytes) => first_grant = first_grant.or(Some(bytes)),
            other => panic!("unexpected frame: {other:?}"),
        }
    }
    assert_eq!(first_grant, Some(STDIN_WINDOW));
    let pid: i32 = String::from_utf8_lossy(&seen)
        .trim()
        .parse()
        .expect("pid line");

    // Spend the whole window. The pipe to the command takes the first part of
    // it and that much is granted back; a command that reads nothing is never
    // granted more than its pipe holds.
    let chunk = Message::Stdin(vec![b'x'; 64 * 1024]).encode();
    for _ in 0..(STDIN_WINDOW as usize / (64 * 1024)) {
        host.write_all(&chunk).await.unwrap();
    }
    let mut regranted = 0u32;
    while let Ok(Ok(Message::StdinWindow(bytes))) = tokio::time::timeout(
        Duration::from_millis(500),
        Message::read_from_async(&mut host),
    )
    .await
    {
        regranted += bytes;
    }
    assert!(
        regranted < STDIN_WINDOW,
        "a command that reads nothing was granted {regranted} more bytes"
    );

    drop(host);
    tokio::time::timeout(Duration::from_secs(10), session)
        .await
        .expect("session did not end after the host disconnected")
        .expect("session task");
    assert_group_dies(pid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_window_reopens_as_the_command_reads() {
    // Four windows' worth of input, sent only against grants, all arrives.
    let total = STDIN_WINDOW as usize * 4;
    let (mut host, session) = start(spec(&["wc", "-c"], false, true));
    let (mut stdout, mut exit) = (Vec::new(), None);
    let (mut credit, mut sent, mut ended) = (0usize, 0usize, false);
    while exit.is_none() {
        while credit > 0 && sent < total {
            let n = credit.min(32 * 1024).min(total - sent);
            host.write_all(&Message::Stdin(vec![b'w'; n]).encode())
                .await
                .unwrap();
            credit -= n;
            sent += n;
        }
        if sent == total && !ended {
            host.write_all(&Message::StdinEof.encode()).await.unwrap();
            ended = true;
        }
        match Message::read_from_async(&mut host).await.expect("frame") {
            Message::StdinWindow(bytes) => credit += bytes as usize,
            Message::Data(data) => stdout.extend_from_slice(&data),
            Message::Exit(code) => exit = Some(code),
            other => panic!("unexpected frame: {other:?}"),
        }
    }
    assert_eq!(String::from_utf8_lossy(&stdout).trim(), total.to_string());
    assert_eq!(exit, Some(0));
    assert_eq!(session.await.unwrap(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn stdin_beyond_the_window_ends_the_session() {
    // A host that ignores the window would make fc-agent hold input without
    // bound. It is treated like a host that went away.
    let (mut host, session) = start(sh("echo $$; exec sleep 60", false, true));
    let pid = read_pid(&mut host).await;
    // The window plus more than any pipe to the command can hold (1 MiB is the
    // kernel's ceiling), so re-grants cannot keep up and the overrun is certain.
    let too_much = Message::Stdin(vec![b'x'; 64 * 1024]).encode();
    for _ in 0..((STDIN_WINDOW as usize + (2 << 20)) / (64 * 1024)) {
        if host.write_all(&too_much).await.is_err() {
            break;
        }
    }
    tokio::time::timeout(Duration::from_secs(10), session)
        .await
        .expect("session did not end after the window was overrun")
        .expect("session task");
    assert_group_dies(pid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_explicit_term_wins_over_the_default() {
    let mut session = spec(&["printenv", "TERM"], true, false);
    session.env = vec![("TERM".to_string(), "screen-256color".to_string())];
    let outcome = run(session, vec![]).await;
    assert_eq!(outcome.stdout, b"screen-256color\r\n");

    let mut session = spec(&["printenv", "TERM"], false, false);
    session.env = vec![("TERM".to_string(), "vt100".to_string())];
    let outcome = run(session, vec![]).await;
    assert_eq!(outcome.stdout, b"vt100\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_final_drain_is_bounded_by_what_was_buffered_at_exit() {
    // A background process that inherited the output keeps refilling the pipe.
    // The drain runs once the command has exited; it delivers what the pipe
    // held at that moment and must not follow the refills.
    let (rx, tx) = nix::unistd::pipe2(nix::fcntl::OFlag::O_NONBLOCK).expect("pipe");
    let mut held = 0usize;
    while let Ok(n) = nix::unistd::write(&tx, &[b'a'; 1000]) {
        held += n;
    }
    assert!(held >= 4096, "pipe took only {held} bytes");

    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let refiller = std::thread::spawn({
        let stop = stop.clone();
        move || {
            while !stop.load(Ordering::Acquire) {
                // Full most of the time; every read by the drain makes room.
                let _ = nix::unistd::write(&tx, &[b'b'; 1000]);
            }
        }
    });

    let (ours, theirs) = UnixStream::pair().expect("socket pair");
    let mut host = AsyncFdStream::new(OwnedFd::from(theirs)).expect("register reader");
    let reader = tokio::spawn(async move {
        let mut outcome = Outcome {
            stdout: Vec::new(),
            stderr: Vec::new(),
            exit: None,
            elapsed: Duration::ZERO,
        };
        collect(&mut host, &mut outcome).await;
        outcome.stdout
    });
    let writer = frame_writer_for_test(OwnedFd::from(ours));
    let mut buf = vec![0u8; 512];
    let drained = tokio::time::timeout(
        Duration::from_secs(5),
        drain(rx.as_raw_fd(), OutputStream::Data, &writer, &mut buf),
    )
    .await;
    stop.store(true, Ordering::Release);
    drop(writer);
    assert!(drained.is_ok(), "the drain followed the refills");
    let delivered = reader.await.expect("reader task");
    assert_eq!(
        delivered.len(),
        held,
        "delivered more or less than was buffered"
    );
    refiller.join().expect("refiller thread");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_final_drain_delivers_everything_buffered_at_exit() {
    // Positive control for the bound: what the command wrote before it exited
    // is all delivered, across many reads.
    let (rx, tx) = nix::unistd::pipe2(nix::fcntl::OFlag::O_NONBLOCK).expect("pipe");
    // Fill the pipe and count what it took. Its capacity is not ours to assume:
    // the kernel shrinks a new pipe to two pages once the user holds many.
    let mut written = Vec::new();
    while let Ok(n) = nix::unistd::write(&tx, &[b'z'; 1000]) {
        written.extend_from_slice(&[b'z'; 1000][..n]);
    }
    assert!(
        written.len() >= 4096,
        "pipe took only {} bytes",
        written.len()
    );

    let (ours, theirs) = UnixStream::pair().expect("socket pair");
    let mut host = AsyncFdStream::new(OwnedFd::from(theirs)).expect("register reader");
    let reader = tokio::spawn(async move {
        let mut outcome = Outcome {
            stdout: Vec::new(),
            stderr: Vec::new(),
            exit: None,
            elapsed: Duration::ZERO,
        };
        collect(&mut host, &mut outcome).await;
        outcome.stdout
    });

    let writer = frame_writer_for_test(OwnedFd::from(ours));
    // A buffer far smaller than the pipe, so the drain has to loop.
    let mut buf = vec![0u8; 512];
    drain(rx.as_raw_fd(), OutputStream::Data, &writer, &mut buf).await;
    drop(writer);

    assert_eq!(reader.await.expect("reader task"), written);
    drop(tx);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_send_never_leaves_part_of_a_frame_on_the_wire() {
    // The session aborts its helper tasks when the command exits, and one of
    // them sends window grants. A sender cut off while the host is slow must
    // leave whole frames only, or the host misreads the Exit that follows.
    let (ours, theirs) = UnixStream::pair().expect("socket pair");
    let writer = frame_writer_for_test(OwnedFd::from(ours));

    // Nobody reads yet, so after the socket buffer and the writer's queue are
    // full this sender is parked, with frames of its own still to send.
    const FRAME: usize = 1 << 20;
    const FRAMES: usize = 8;
    let parked = {
        let writer = writer.clone();
        tokio::spawn(async move {
            for _ in 0..FRAMES {
                if send(&writer, &Message::Data(vec![b'D'; FRAME]))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!parked.is_finished(), "the sender was never held back");
    parked.abort();
    let _ = parked.await;

    let exit = tokio::spawn({
        let writer = writer.clone();
        async move { send(&writer, &Message::Exit(7)).await }
    });

    // Whatever arrives must parse as whole frames, ending in the Exit.
    let mut host = AsyncFdStream::new(OwnedFd::from(theirs)).expect("register reader");
    let mut frames = Vec::new();
    let ended = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            match Message::read_from_async(&mut host).await {
                Ok(Message::Exit(code)) => return Some(code),
                Ok(Message::Data(data)) => frames.push(data.len()),
                Ok(other) => panic!("unexpected frame: {other:?}"),
                Err(_) => return None,
            }
        }
    })
    .await;
    assert_eq!(
        ended,
        Ok(Some(7)),
        "the stream after a cancelled send did not parse (data frames seen: {frames:?})"
    );
    assert!(
        !frames.is_empty() && frames.len() < FRAMES && frames.iter().all(|len| *len == FRAME),
        "expected some whole frames and not all of them, got {frames:?}"
    );
    let _ = exit.await;
}

/// Host side of a session whose stdin the test streams while it collects.
async fn run_streaming(spec: SessionSpec, input: Vec<u8>) -> Outcome {
    let started = Instant::now();
    let (mut host, session) = start(spec);
    let mut sender = host.handle();
    let sending = tokio::spawn(async move {
        let _ = sender.write_all(&input).await;
    });
    let mut outcome = Outcome {
        stdout: Vec::new(),
        stderr: Vec::new(),
        exit: None,
        elapsed: Duration::ZERO,
    };
    tokio::time::timeout(Duration::from_secs(60), collect(&mut host, &mut outcome))
        .await
        .expect("session did not finish within 60s");
    outcome.elapsed = started.elapsed();
    sending.abort();
    let _ = session.await;
    outcome
}

#[tokio::test(flavor = "multi_thread")]
async fn many_small_stdin_frames_within_the_window_do_not_end_the_session() {
    // A producer that writes a byte at a time, into a command that is not
    // reading yet: 200,000 frames, and fewer bytes than the window.
    const FRAMES: usize = 200_000;
    assert!(FRAMES < STDIN_WINDOW as usize);
    let one = Message::Stdin(vec![b'x']).encode();
    let input: Vec<u8> = one
        .iter()
        .copied()
        .cycle()
        .take(one.len() * FRAMES)
        .collect();
    let outcome = run_streaming(sh("sleep 2; exit 7", false, true), input).await;
    assert_eq!(
        outcome.exit,
        Some(7),
        "a host that stayed inside its window was treated as gone"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_resize_is_applied_while_the_command_is_not_reading_stdin() {
    // Lines, so the terminal's input queue fills and the stdin write blocks.
    let mut input = Message::Stdin(b"0123456\n".repeat(8 * 1024)).encode();
    input.extend(
        Message::Resize(exec_proto::TtySize {
            rows: 31,
            cols: 101,
        })
        .encode(),
    );
    let outcome = run_streaming(sh("sleep 2; stty size", true, true), input).await;
    let text = String::from_utf8_lossy(&outcome.stdout);
    assert!(
        text.contains("31 101"),
        "the resize waited behind blocked stdin: {:?}",
        &text[text.len().saturating_sub(80)..]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn frames_a_host_has_no_business_sending_are_ignored() {
    let outcome = run(
        spec(&["cat"], false, true),
        vec![
            Message::Data(b"not yours to send".to_vec()),
            Message::Exit(9),
            Message::StdinWindow(1 << 30),
            Message::Stdin(Vec::new()),
            Message::Stdin(b"hi\n".to_vec()),
            Message::StdinEof,
            Message::StdinEof,
            Message::Stdin(b"after the end\n".to_vec()),
        ],
    )
    .await;
    assert_eq!(outcome.stdout, b"hi\n");
    assert_eq!(outcome.exit, Some(0));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_stdin_window_is_the_first_frame_of_an_interactive_session() {
    // The host takes output before any window as the mark of an fc-agent that
    // predates flow control, so the grant must precede everything.
    for tty in [false, true] {
        let (mut host, session) = start(sh("echo early", tty, true));
        let first =
            tokio::time::timeout(Duration::from_secs(20), Message::read_from_async(&mut host))
                .await
                .expect("no frame within 20s")
                .expect("read the first frame");
        assert!(
            matches!(first, Message::StdinWindow(bytes) if bytes == STDIN_WINDOW),
            "tty={tty}: the first frame was {first:?}"
        );
        drop(host);
        let _ = session.await;
    }
}

/// The frame writer the session uses, over `fd`.
fn frame_writer_for_test(fd: OwnedFd) -> FrameWriter {
    FrameWriter::start(AsyncFdStream::new(fd).expect("register writer")).0
}
