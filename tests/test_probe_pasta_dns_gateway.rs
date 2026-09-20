//! `scripts/probe-pasta-dns-gateway.sh` must not outlive itself.
//!
//! The probe starts two Python DNS responders inside a private user, network
//! and mount namespace. Neither ever returns: each loops on `recvfrom`
//! forever. Nothing in the script ends them, and a non-interactive shell does
//! not reap background jobs on exit, so every run used to leave two processes
//! holding those namespaces open. The probe is meant to be cheap enough to run
//! whenever the pasta wiring is in question, which is exactly the usage that
//! accumulates them.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Every tool the probe and this test shell out to. A missing one means the
/// test could not evaluate anything, which must block rather than pass.
fn require_tools() {
    for tool in ["unshare", "ip", "python3", "bash", "mount"] {
        let found = Command::new("sh")
            .arg("-c")
            .arg(format!("command -v {tool} >/dev/null 2>&1"))
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(
            found,
            "BLOCKED: '{tool}' is missing; this test cannot evaluate anything"
        );
    }
    // The probe's own namespace. Without unprivileged user namespaces it
    // cannot start at all, and neither can rootless fcvm.
    let status = Command::new("unshare")
        .args([
            "--user",
            "--map-root-user",
            "--net",
            "--mount",
            "--fork",
            "--",
            "true",
        ])
        .status()
        .expect("run unshare");
    assert!(
        status.success(),
        "BLOCKED: this box cannot create a user+net+mount namespace, so the probe \
         cannot run here (kernel.unprivileged_userns_clone / \
         kernel.apparmor_restrict_unprivileged_userns)"
    );
}

fn write_exec(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    let mut perms = std::fs::metadata(path).expect("stat stub").permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(path, perms).expect("chmod stub");
}

/// The two commands the probe reaches the guest with, stubbed.
///
/// `nsenter -t <pid> -U -n -- <cmd>` runs twice per `ask`: once to wait for
/// pasta0 to carry the guest address, once to ask the resolver. Both are
/// answered here, the second from a counter so the two arms differ, because a
/// real answer needs the VM-less pasta wiring these tests are not measuring.
/// `dig` only has to exist: the probe's tool check looks it up before the
/// namespace, and the query itself goes through the `nsenter` stub.
fn write_guest_stubs(bin: &Path, calls: &Path) {
    write_exec(
        &bin.join("nsenter"),
        &format!(
            "#!/bin/bash\n\
             for a in \"$@\"; do\n\
             \tcase \"$a\" in\n\
             \t\tdig) n=$(cat {calls} 2>/dev/null || echo 0); n=$((n + 1)); echo \"$n\" >{calls}\n\
             \t\t\t[ \"$n\" = 1 ] && echo 10.0.2.2 || echo 203.0.113.99\n\
             \t\t\texit 0 ;;\n\
             \t\tpasta0) echo '2: pasta0    inet 10.0.2.100/24 scope global pasta0' ; exit 0 ;;\n\
             \tesac\n\
             done\n\
             exit 0\n",
            calls = calls.display()
        ),
    );
    write_exec(&bin.join("dig"), "#!/bin/bash\nexit 0\n");
}

/// Processes whose command line mentions `needle`, by host pid.
fn processes_matching(needle: &str) -> Vec<String> {
    let mut found = Vec::new();
    let entries = std::fs::read_dir("/proc").expect("read /proc");
    for entry in entries.flatten() {
        let name = entry.file_name();
        let pid = match name.to_string_lossy().parse::<u32>() {
            Ok(pid) => pid,
            Err(_) => continue,
        };
        let cmdline = match std::fs::read(format!("/proc/{pid}/cmdline")) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).replace('\0', " "),
            Err(_) => continue,
        };
        if cmdline.contains(needle) {
            found.push(format!("{pid}: {}", cmdline.trim()));
        }
    }
    found
}

/// How long one run of the probe may take before a test gives up on it.
///
/// Measured with the stubs these tests use: 0.190, 0.200 and 0.189 s on a box
/// with a 1-minute load average of 17. A helper the probe cannot end costs
/// 60 s per `ask`, because both helpers are `sleep 60`, so 30 s is 150 times
/// the healthy run and half of the shortest stall.
const PROBE_DEADLINE: Duration = Duration::from_secs(30);

/// Whether a `SigIgn` mask from `/proc/<pid>/status` has SIGTERM in it.
fn sigign_has_sigterm(hex: &str) -> bool {
    u64::from_str_radix(hex.trim(), 16)
        .map(|mask| mask & (1 << (libc::SIGTERM - 1)) != 0)
        .unwrap_or(false)
}

/// One line per process in process group `pgid`: pid, state, the kernel
/// function it sleeps in, whether it ignores SIGTERM, and its command line.
///
/// By group rather than by ancestry, so a process whose parent already exited
/// is still listed. The command line is skipped for a process in
/// uninterruptible sleep, where reading it can block on the target's mm.
fn describe_process_group(pgid: u32) -> String {
    let mut lines = Vec::new();
    for entry in std::fs::read_dir("/proc").expect("read /proc").flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        // "pid (comm) state ppid pgrp ...", where comm may hold spaces and
        // parentheses, so split at the last one.
        let Some((head, tail)) = stat.rsplit_once(')') else {
            continue;
        };
        let mut fields = tail.split_whitespace();
        let state = fields.next().unwrap_or("?").to_string();
        if fields.nth(1).and_then(|f| f.parse::<u32>().ok()) != Some(pgid) {
            continue;
        }
        let sigterm = std::fs::read_to_string(format!("/proc/{pid}/status"))
            .ok()
            .and_then(|status| {
                status
                    .lines()
                    .find_map(|l| l.strip_prefix("SigIgn:").map(sigign_has_sigterm))
            });
        let sigterm = match sigterm {
            Some(true) => "SIGTERM ignored",
            Some(false) => "SIGTERM not ignored",
            None => "SIGTERM unknown",
        };
        let wchan = std::fs::read_to_string(format!("/proc/{pid}/wchan")).unwrap_or_default();
        let command = if state == "D" {
            None
        } else {
            std::fs::read(format!("/proc/{pid}/cmdline"))
                .ok()
                .map(|bytes| String::from_utf8_lossy(&bytes).replace('\0', " "))
                .filter(|c| !c.trim().is_empty())
        };
        let command = command.unwrap_or_else(|| {
            let comm = head.split_once('(').map(|(_, c)| c).unwrap_or("?");
            format!("[{comm}]")
        });
        lines.push(format!(
            "  {pid} {state} {} ({sigterm}): {}",
            if wchan.is_empty() { "-" } else { wchan.trim() },
            command.trim()
        ));
    }
    lines.sort();
    lines.join("\n")
}

fn drain<R: Read + Send + 'static>(mut pipe: R) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    rx
}

/// Runs the probe to its end, or fails once `PROBE_DEADLINE` has passed.
///
/// `Command::output` waits for as long as the script does, so a probe that
/// stalls becomes the test runner's timeout, with "running 1 test" as the only
/// captured output and nothing about what it was waiting for. Here a stalled
/// run is described, killed and reported instead.
///
/// The probe gets its own process group so that the whole run can be listed
/// and killed at once. Killing the outer shell alone would leave the `unshare`
/// under it alive, and the namespace's init dies only when that `unshare` does.
fn run_probe(cmd: &mut Command) -> Output {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .expect("run the probe");
    let pgid = child.id();
    let stdout = drain(child.stdout.take().expect("probe stdout"));
    let stderr = drain(child.stderr.take().expect("probe stderr"));
    // After the run is over, or killed, every holder of the pipes is gone and
    // the readers finish at once. The bound is for a holder that is not.
    let collect = |rx: &mpsc::Receiver<Vec<u8>>| {
        rx.recv_timeout(Duration::from_secs(5))
            .map(|bytes| String::from_utf8_lossy(&bytes).to_string())
    };

    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait for the probe") {
            break status;
        }
        if started.elapsed() >= PROBE_DEADLINE {
            let group = describe_process_group(pgid);
            unsafe { libc::killpg(pgid as i32, libc::SIGKILL) };
            let _ = child.wait();
            panic!(
                "the probe was still running {}s after it started, and it finishes in \
                 about 0.2s. Its process group when it was killed (pid, state, kernel \
                 wait, SIGTERM, command):\n{group}\n\
                 A `sleep 60` that ignores SIGTERM is a helper the probe signalled and \
                 is waiting out.\nstdout:\n{}\nstderr:\n{}",
                PROBE_DEADLINE.as_secs(),
                collect(&stdout).unwrap_or_default(),
                collect(&stderr).unwrap_or_default(),
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    };

    match (collect(&stdout), collect(&stderr)) {
        (Ok(stdout), Ok(stderr)) => Output {
            status,
            stdout: stdout.into_bytes(),
            stderr: stderr.into_bytes(),
        },
        _ => {
            let group = describe_process_group(pgid);
            unsafe { libc::killpg(pgid as i32, libc::SIGKILL) };
            panic!(
                "the probe exited with {status} and something it started still holds \
                 its output open. Its process group (pid, state, kernel wait, SIGTERM, \
                 command):\n{group}"
            );
        }
    }
}

/// With no `PASTA_BIN`, the probe must refuse rather than pick a binary out of
/// a directory listing.
///
/// Which pasta fcvm runs is a function of the ACTIVE config: the binary is
/// content-addressed under `paths.assets_dir`, and the config that names both
/// is found through a lookup chain this script cannot replicate. A listing
/// answers a different question, so the probe would report on an artifact that
/// is not the one under review.
///
/// The situation is built rather than waited for: a tmpfs over `/mnt` inside a
/// private mount namespace, holding one plausible-looking `pasta-*.bin` that
/// records nothing and sleeps. Everything else is the shipped script.
///
/// RED BEFORE THE FIX:
///
/// ```text
/// assertion `left == right` failed: the probe resolved a pasta out of the
/// assets directory and ran the whole thing: exit status: 0
/// stdout:
/// OK   with -D none:    10.0.2.2 (the replay on host 127.0.0.1:53 answered)
/// OK   without it:      203.0.113.99 (pasta redirected port 53 to the host's own resolver)
///   left: Some(0)
///  right: Some(2)
/// ```
///
/// Two OK lines: a verdict about a binary the caller never named.
#[test]
fn the_probe_refuses_to_guess_which_pasta_to_run() {
    require_tools();
    let script = repo_root().join("scripts/probe-pasta-dns-gateway.sh");
    let tmp = tempfile::tempdir().expect("temp dir");
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).expect("create stub bin");
    write_guest_stubs(&bin, &tmp.path().join("dig-calls"));
    let work_root = tmp.path().join("work");
    std::fs::create_dir_all(&work_root).expect("create work root");
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    // A private /mnt so the plant is this test's, and the box's real assets
    // directory is neither read nor written.
    let plant = "set -e\n\
         mount -t tmpfs none /mnt\n\
         mkdir -p /mnt/fcvm-btrfs/pasta\n\
         printf '#!/bin/sh\\nexec sleep 60\\n' >/mnt/fcvm-btrfs/pasta/pasta-stale.bin\n\
         chmod +x /mnt/fcvm-btrfs/pasta/pasta-stale.bin\n\
         exec bash \"$0\"\n";
    let output = run_probe(
        Command::new("unshare")
            .args([
                "--user",
                "--map-root-user",
                "--mount",
                "--fork",
                "--",
                "bash",
                "-c",
                plant,
            ])
            .arg(&script)
            .env("PATH", &path)
            .env("TMPDIR", &work_root)
            .env_remove("PASTA_BIN"),
    );
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    assert_eq!(
        output.status.code(),
        Some(2),
        "the probe resolved a pasta out of the assets directory and ran the \
         whole thing: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status,
    );
    assert!(
        !stdout.contains("OK ") && !stdout.contains("FAIL "),
        "the probe rendered a verdict without being told which binary to \
         test:\nstdout:\n{stdout}"
    );
    assert!(
        stderr.contains("PASTA_BIN"),
        "a refusal must name what the caller has to supply:\nstderr:\n{stderr}"
    );
}

/// A directory passes `test -x`. The `PASTA_BIN` guard must still refuse it.
///
/// `-x` asks whether the caller may execute the path, and for a directory that
/// means traverse it, so every directory this test can enter satisfies it. A
/// `PASTA_BIN` naming one therefore walked past the documented exit-2 path and
/// the run carried on: private namespace, veth pair, both DNS responders, and
/// only then an invocation of a path that can never be a program. The caller
/// got back a verdict about DNS with no mention of what they had supplied.
///
/// The guard sits ahead of all of that, so a refusal needs none of the stubs
/// below; they are here to keep the pre-fix run bounded.
///
/// RED BEFORE THE FIX:
///
/// ```text
/// assertion `left == right` failed: a directory passed the guard and the probe
/// ran the whole thing: exit status: 0
/// stdout:
/// OK   with -D none:    10.0.2.2 (the replay on host 127.0.0.1:53 answered)
/// OK   without it:      203.0.113.99 (pasta redirected port 53 to the host's own resolver)
///   left: Some(0)
///  right: Some(2)
/// ```
///
/// Two OK lines about a pasta that was never executed.
#[test]
fn the_probe_refuses_a_pasta_bin_that_is_not_a_regular_file() {
    require_tools();
    let script = repo_root().join("scripts/probe-pasta-dns-gateway.sh");
    let tmp = tempfile::tempdir().expect("temp dir");
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).expect("create stub bin");
    write_guest_stubs(&bin, &tmp.path().join("dig-calls"));
    let work_root = tmp.path().join("work");
    std::fs::create_dir_all(&work_root).expect("create work root");
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    // What the caller hands over: a directory where a binary was meant to be,
    // which is what a truncated path or an assets directory produces.
    let dir = tmp.path().join("pasta-is-a-directory");
    std::fs::create_dir_all(&dir).expect("create the directory to hand the guard");

    let output = run_probe(
        Command::new("bash")
            .arg(&script)
            .env("PATH", &path)
            .env("TMPDIR", &work_root)
            .env("PASTA_BIN", &dir),
    );
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    // A probe that stopped on a missing tool exits 2 for a reason that says
    // nothing about the guard, so the assertion below would pass vacuously.
    assert!(
        !stderr.contains("this probe cannot evaluate anything"),
        "BLOCKED: the probe stopped on a missing tool, so this says nothing \
         about the guard:\nstderr:\n{stderr}"
    );
    assert_eq!(
        output.status.code(),
        Some(2),
        "a directory passed the guard and the probe ran the whole thing: {}\n\
         stdout:\n{stdout}\nstderr:\n{stderr}",
        output.status,
    );
    assert!(
        !stdout.contains("OK ") && !stdout.contains("FAIL "),
        "the probe rendered a verdict about a path that is not a \
         program:\nstdout:\n{stdout}"
    );
    assert!(
        stderr.contains(&dir.to_string_lossy().to_string()),
        "a refusal must name the path it was given:\nstderr:\n{stderr}"
    );
}

/// A namespace the kernel refuses must not leave a work directory behind.
///
/// `mktemp -d` runs in the OUTER shell, so the directory exists on the host
/// before `unshare` starts, while the `trap` that removes it is installed by
/// the `--inside` shell. An `unshare` that fails, on a box with unprivileged
/// user namespaces switched off or under a restrictive AppArmor profile, never
/// reaches that trap, so every attempt left one more directory in TMPDIR. The
/// probe is meant to be cheap to re-run while the pasta wiring is in question,
/// which is exactly the usage that accumulates them.
///
/// `unshare` is stubbed to fail, because the alternative is turning off
/// unprivileged user namespaces on the host running the test.
///
/// RED BEFORE THE FIX:
///
/// ```text
/// the probe left 1 director(ies) in its TMPDIR after exiting with exit status: 1:
/// tmp.qkV3rXo1aE
/// ```
#[test]
fn the_probe_leaves_no_work_directory_when_the_namespace_fails() {
    require_tools();
    let script = repo_root().join("scripts/probe-pasta-dns-gateway.sh");
    let tmp = tempfile::tempdir().expect("temp dir");
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).expect("create stub bin");
    write_guest_stubs(&bin, &tmp.path().join("dig-calls"));
    // The kernel's refusal, without having to reconfigure the host. The tool
    // check ahead of it only asks whether `unshare` is on PATH.
    write_exec(
        &bin.join("unshare"),
        "#!/bin/bash
exit 1
",
    );
    write_exec(
        &bin.join("pasta"),
        "#!/bin/bash
exec sleep 60
",
    );
    let work_root = tmp.path().join("work");
    std::fs::create_dir_all(&work_root).expect("create work root");
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let output = run_probe(
        Command::new("bash")
            .arg(&script)
            .env("PATH", &path)
            .env("TMPDIR", &work_root)
            .env("PASTA_BIN", bin.join("pasta")),
    );
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    // A probe that stopped before `mktemp -d` leaves nothing behind for a
    // reason that says nothing about the trap.
    assert!(
        !stderr.contains("this probe cannot evaluate anything") && !stderr.contains("BLOCKED:"),
        "BLOCKED: the probe stopped before it reached the namespace, so this \
         says nothing about the work directory:\nstderr:\n{stderr}"
    );
    assert!(
        !output.status.success(),
        "the stubbed unshare was supposed to fail the run:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let left: Vec<String> = std::fs::read_dir(&work_root)
        .expect("read work root")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert!(
        left.is_empty(),
        "the probe left {} director(ies) in its TMPDIR after exiting with {}:\n{}",
        left.len(),
        output.status,
        left.join("\n"),
    );
}

/// The probe runs to its normal end and leaves nothing behind.
///
/// The pasta binary, the namespace entry and the query are stubbed: what is
/// under test is the lifetime of the responders the probe starts, not the
/// verdict it renders about pasta. Everything else is the shipped script,
/// including the namespace it builds and the exit path it takes.
///
/// RED BEFORE THE FIX: `the probe left 2 process(es) alive after exiting with
/// exit status: 0`, naming `responder.py 127.0.0.53 203.0.113.99
/// host-resolver` and `responder.py 127.0.0.1 10.0.2.2 replay`, both still
/// running under the probe's own work directory after it printed its two OK
/// lines.
#[test]
fn the_probe_leaves_no_dns_responder_behind() {
    require_tools();
    let script = repo_root().join("scripts/probe-pasta-dns-gateway.sh");
    assert!(
        script.is_file(),
        "the probe is gone from {}",
        script.display()
    );

    let tmp = tempfile::tempdir().expect("temp dir");
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).expect("create stub bin");

    // A pasta that stays up until the probe kills it, so the probe follows its
    // ordinary path through both `ask` calls.
    write_exec(&bin.join("pasta"), "#!/bin/bash\nexec sleep 60\n");
    write_guest_stubs(&bin, &tmp.path().join("dig-calls"));

    // The probe's work directory comes from `mktemp -d`, so pointing TMPDIR
    // here makes every responder's command line carry this path and nothing
    // else on the box does.
    let work_root = tmp.path().join("work");
    std::fs::create_dir_all(&work_root).expect("create work root");
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = run_probe(
        Command::new("bash")
            .arg(&script)
            .env("PATH", &path)
            .env("TMPDIR", &work_root)
            .env("PASTA_BIN", bin.join("pasta")),
    );

    // A probe that never got as far as starting the responders leaves nothing
    // behind for a reason that says nothing about reaping, so the assertion
    // below would pass vacuously. Both OK lines mean it ran to its normal end.
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success()
            && stdout.contains("OK   with -D none")
            && stdout.contains("OK   without it"),
        "BLOCKED: the probe did not reach its normal end, so this says nothing \
         about reaping. status {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status,
    );

    let needle = work_root.to_string_lossy().to_string();
    // The kernel tears the subtree down as the namespace's init exits; give it
    // a moment rather than racing the exit.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut alive = processes_matching(&needle);
    while !alive.is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
        alive = processes_matching(&needle);
    }
    for line in &alive {
        // Do not leave a leak behind for the next test to trip over.
        if let Some(pid) = line.split(':').next().and_then(|p| p.parse::<i32>().ok()) {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
    assert!(
        alive.is_empty(),
        "the probe left {} process(es) alive after exiting with {}:\n{}\nstdout:\n{stdout}",
        alive.len(),
        output.status,
        alive.join("\n"),
    );
}

/// A helper that ignores SIGTERM must not hold the probe up.
///
/// `ask` ends its two helpers, pasta and the `sleep 60` holding the guest's
/// namespace, and then waits for both. It ended them with the default SIGTERM,
/// which a process that started with SIGTERM ignored never acts on, so the
/// `wait` lasted until the `sleep` ran out: 60 s per `ask`, 120 s for a probe
/// that otherwise takes 0.2 s.
///
/// Whether SIGTERM is ignored in there is not the probe's to decide. An ignored
/// signal is inherited through every fork and exec, and a bash script cannot
/// reset one it started with. `unshare --fork` from util-linux 2.36 and 2.37
/// (RHEL 9, Ubuntu 22.04, Debian 11) sets SIGINT and SIGTERM to SIG_IGN before
/// it forks and puts them back only in the parent, so on those hosts every run
/// of the probe is in this state. 2.38 blocks the two signals in the parent and
/// restores the mask in the child instead. A caller that ignores SIGTERM gives
/// the same result under any util-linux, which is how this test builds it.
///
/// RED BEFORE THE FIX, 30 s into a run the fixed probe finishes in 0.2 s (paths
/// shortened, the four responder processes left out):
///
/// ```text
/// the probe was still running 30s after it started, and it finishes in about
/// 0.2s. Its process group when it was killed (pid, state, kernel wait,
/// SIGTERM, command):
///   220716 S do_wait (SIGTERM ignored): bash scripts/probe-pasta-dns-gateway.sh
///   220727 S do_wait (SIGTERM ignored): unshare --user --map-root-user --net --mount --pid --mount-proc --kill-child --fork -- scripts/probe-pasta-dns-gateway.sh --inside ...
///   220734 S anon_pipe_read (SIGTERM ignored): bash scripts/probe-pasta-dns-gateway.sh --inside ...
///   220791 S do_wait (SIGTERM ignored): bash scripts/probe-pasta-dns-gateway.sh --inside ...
///   220792 S hrtimer_nanosleep (SIGTERM ignored): sleep 60
///   220797 S hrtimer_nanosleep (SIGTERM ignored): sleep 60
/// ```
///
/// The last three lines are the stall: the `ask` subshell in `wait4`, and under
/// it the two helpers it had already signalled, both still asleep.
#[test]
fn the_probe_does_not_wait_out_a_helper_that_ignores_sigterm() {
    require_tools();
    let script = repo_root().join("scripts/probe-pasta-dns-gateway.sh");
    let tmp = tempfile::tempdir().expect("temp dir");
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).expect("create stub bin");

    // A pasta that stays up until the probe ends it, and that first records
    // its own SigIgn, so the test can tell whether SIGTERM really was ignored
    // where the probe's helpers run.
    let sigign = tmp.path().join("pasta-sigign");
    write_exec(
        &bin.join("pasta"),
        &format!(
            "#!/bin/bash\n\
             while read -r key value; do\n\
             \t[ \"$key\" = SigIgn: ] && echo \"$value\" >{sigign}\n\
             done </proc/self/status\n\
             exec sleep 60\n",
            sigign = sigign.display()
        ),
    );
    write_guest_stubs(&bin, &tmp.path().join("dig-calls"));
    let work_root = tmp.path().join("work");
    std::fs::create_dir_all(&work_root).expect("create work root");
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let mut cmd = Command::new("bash");
    cmd.arg(&script)
        .env("PATH", &path)
        .env("TMPDIR", &work_root)
        .env("PASTA_BIN", bin.join("pasta"));
    // An ignored signal survives exec, so this reaches the script, the
    // namespace and both helpers.
    unsafe {
        cmd.pre_exec(|| {
            if libc::signal(libc::SIGTERM, libc::SIG_IGN) == libc::SIG_ERR {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let output = run_probe(&mut cmd);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    // A helper that did not ignore SIGTERM dies on any signal, so a run where
    // the ignore never arrived passes for a reason that says nothing here.
    let recorded = std::fs::read_to_string(&sigign).unwrap_or_default();
    assert!(
        sigign_has_sigterm(&recorded),
        "BLOCKED: the stub pasta did not start with SIGTERM ignored (SigIgn {recorded:?}), \
         so this says nothing about a helper that ignores it.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        output.status.success()
            && stdout.contains("OK   with -D none")
            && stdout.contains("OK   without it"),
        "the probe did not reach its normal end with SIGTERM ignored. status {}\n\
         stdout:\n{stdout}\nstderr:\n{stderr}",
        output.status,
    );
}
