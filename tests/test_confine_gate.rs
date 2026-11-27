//! scripts/confine-gate.sh, the wrapper `make test-confine` runs its lane through: each
//! thing it must fail on, against a stand-in lane that prints what nextest prints and
//! writes the records tests/common writes. The script needs no VM and no setup, so this
//! runs in the unit lanes. So does the API call the gate's assertion makes, against
//! stand-in listeners.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

/// What a stand-in lane does with one test's per-test log.
#[derive(Clone, Copy)]
enum Log {
    /// The log is written with this line and listed for the gate.
    Line(&'static str),
    /// The log is listed and is not there to read.
    Unreadable,
    /// The test opens no log.
    None,
}

/// One run of the gate over a stand-in lane.
struct Lane {
    /// Tests the lane reports as passed, each with its log.
    passed: Vec<(&'static str, Log)>,
    /// The confinement assertions recorded: test, then `healthy` or `seen`.
    asserted: Vec<(&'static str, &'static str)>,
    /// Extra lines the lane prints.
    output: Vec<&'static str>,
    /// A log written to the directory during the lane that no test listed: another lane's,
    /// or one a test keeps for itself.
    foreign_log: Option<&'static str>,
    /// What the tests' API calls recorded before they stopped their VMs.
    api: Vec<&'static str>,
    exit: i32,
    min: usize,
    no_vmm: &'static str,
    boot_fails: &'static str,
}

const HEALTHY_LINE: Log = Log::Line("INFO fcvm: VM is healthy");

impl Lane {
    fn clean() -> Self {
        Lane {
            passed: vec![
                ("test_sanity_rootless", HEALTHY_LINE),
                ("test_exec_rootless", HEALTHY_LINE),
                ("test_boot_fails", HEALTHY_LINE),
                ("vm::tests::trap", Log::None),
            ],
            asserted: vec![
                ("test_sanity_rootless", "healthy"),
                ("test_exec_rootless", "seen"),
                ("test_exec_rootless", "healthy"),
                ("test_boot_fails", "seen"),
            ],
            output: Vec::new(),
            foreign_log: None,
            api: vec!["ok\ttest_sanity_rootless", "ok\ttest_exec_rootless"],
            exit: 0,
            min: 4,
            no_vmm: "vm::tests::trap",
            boot_fails: "test_boot_fails",
        }
    }

    /// The gate's exit status and what it printed.
    fn run(&self) -> (i32, String) {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("logs");
        std::fs::create_dir(&logs).unwrap();
        let mut script = String::from("#!/bin/sh\n");
        script.push_str(&format!(
            "echo \"retries=$NEXTEST_RETRIES confine=$FCVM_CONFINE seccomp=$FCVM_CONFINE_SECCOMP\"\n\
             echo '    Starting {} tests across 3 binaries (9 tests skipped)'\n",
            self.passed.len()
        ));
        for (i, (test, log)) in self.passed.iter().enumerate() {
            script.push_str(&format!(
                "echo '        PASS [   1.{i:03}s] ({}/{}) fcvm::bin {test}'\n",
                i + 1,
                self.passed.len()
            ));
            let path = logs.join(format!("{i}.log"));
            match log {
                Log::Line(line) => script.push_str(&format!(
                    "echo '{line}' > '{path}'\n\
                     printf '%s\\t%s\\n' '{test}' '{path}' >> \"$FCVM_CONFINE_LOG_LIST\"\n",
                    path = path.display()
                )),
                Log::Unreadable => script.push_str(&format!(
                    "printf '%s\\t%s\\n' '{test}' '{}' >> \"$FCVM_CONFINE_LOG_LIST\"\n",
                    path.display()
                )),
                Log::None => {}
            }
        }
        for (i, (test, kind)) in self.asserted.iter().enumerate() {
            script.push_str(&format!(
                "printf '%s\\t%s\\t%s\\n' '{test}' '{}' '{kind}' >> \"$FCVM_CONFINE_ASSERT_LOG\"\n",
                4000 + i
            ));
        }
        for line in &self.api {
            script.push_str(&format!(
                "printf '%s\\n' '{line}' >> \"$FCVM_CONFINE_API_LOG\"\n"
            ));
        }
        for line in &self.output {
            script.push_str(&format!("echo '{line}'\n"));
        }
        if let Some(line) = self.foreign_log {
            script.push_str(&format!(
                "echo '{line}' > '{}/another-lane.log'\n",
                logs.display()
            ));
        }
        script.push_str(&format!(
            "echo '     Summary [  12.345s] {n} tests run: {n} passed, 9 skipped'\nexit {}\n",
            self.exit,
            n = self.passed.len()
        ));
        let lane = dir.path().join("lane.sh");
        std::fs::write(&lane, script).unwrap();
        let output = Command::new("bash")
            .arg(gate())
            .arg(self.min.to_string())
            .arg("sh")
            .arg(&lane)
            .env("CONFINE_GATE_LOG_DIR", &logs)
            .env("CONFINE_GATE_NO_VMM_TESTS", self.no_vmm)
            .env("CONFINE_GATE_BOOT_FAILS_TESTS", self.boot_fails)
            .env_remove("FCVM_CONFINE")
            .env_remove("FCVM_CONFINE_SECCOMP")
            .env_remove("NEXTEST_RETRIES")
            .output()
            .expect("running scripts/confine-gate.sh");
        (
            output.status.code().unwrap_or(-1),
            format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ),
        )
    }

    /// This lane with the log of its second test replaced.
    fn with_second_log(log: Log) -> Self {
        let mut lane = Lane::clean();
        lane.passed[1].1 = log;
        lane
    }
}

fn gate() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/confine-gate.sh")
}

fn assert_fails(lane: Lane, needle: &str) {
    let (code, text) = lane.run();
    assert_eq!(code, 1, "the gate did not fail: {text}");
    assert!(text.contains(needle), "missing {needle:?} in: {text}");
    assert!(!text.contains("confine gate: CLEAR"), "{text}");
}

const STOP: &str = "ERROR vm: Firecracker was stopped by its confinement policy: its fc_api \
                    thread made syscall 226";

#[test]
fn a_clean_lane_is_clear_and_runs_confined_enforcing_and_without_retries() {
    let (code, text) = Lane::clean().run();
    assert_eq!(code, 0, "{text}");
    assert!(
        text.contains("retries=0 confine=required seccomp=enforce"),
        "{text}"
    );
    assert!(
        text.contains(
            "confine gate: CLEAR: 4 tests ran once and passed; 3 asserted a VMM confined \
             (4 VMMs; 1 tests start none); no VMM was stopped by the policy in the lane's \
             output or the 3 logs written during it; 2 VMMs answered their API before they \
             were stopped"
        ),
        "{text}"
    );
}

/// A green lane in which a VMM was stopped by the policy: with a retry the test passes on
/// its second try, and a test that expects its VM to fail passes on the first.
#[test]
fn a_policy_stop_fails_a_lane_whose_tests_all_passed() {
    assert_fails(
        Lane::with_second_log(Log::Line(STOP)),
        "a VMM was stopped by the confinement policy",
    );
    // Firecracker's own line, when fcvm's report did not run.
    assert_fails(
        Lane::with_second_log(Log::Line(
            "Shutting down VM after intercepting a bad syscall (226).",
        )),
        "a VMM was stopped by the confinement policy",
    );
    // In the lane's output and in no per-test log.
    assert_fails(
        Lane {
            output: vec!["ERROR vm: Firecracker was stopped by its confinement policy"],
            ..Lane::clean()
        },
        "lane output:",
    );
}

/// A log the gate cannot read is a failure (grep's status for it was discarded and read as
/// "no stop"), and so is a test that listed none: a file in the shared directory does not
/// stand in for it (any file there used to, another lane's included). A file written there
/// during the lane is still scanned, listed or not.
#[test]
fn the_gate_reads_its_own_lanes_logs_and_fails_on_one_it_cannot_read() {
    let (code, text) = Lane::with_second_log(Log::Unreadable).run();
    assert_eq!(code, 1, "{text}");
    assert!(
        text.contains("these per-test logs could not be read") && text.contains("/logs/1.log"),
        "{text}"
    );
    let (code, text) = Lane {
        foreign_log: Some("INFO fcvm: VM is healthy"),
        ..Lane::with_second_log(Log::None)
    }
    .run();
    assert_eq!(code, 1, "{text}");
    assert!(
        text.contains("passed and opened no per-test log") && text.contains("  test_exec_rootless"),
        "{text}"
    );
    assert_fails(
        Lane {
            foreign_log: Some(STOP),
            ..Lane::clean()
        },
        "a VMM was stopped by the confinement policy",
    );
}

#[test]
fn a_lane_that_ran_fewer_tests_than_the_gate_is_pinned_to_fails() {
    assert_fails(
        Lane {
            min: 5,
            ..Lane::clean()
        },
        "ran 4 tests and the gate is pinned to at least 5",
    );
}

#[test]
fn a_test_that_passed_without_asserting_its_vmm_confined_fails_the_gate() {
    let unasserted = "passed without asserting their VMM confined";
    let (code, text) = Lane {
        asserted: vec![
            ("test_sanity_rootless", "healthy"),
            ("test_boot_fails", "seen"),
        ],
        ..Lane::clean()
    }
    .run();
    assert_eq!(code, 1, "{text}");
    assert!(
        text.contains(unasserted) && text.contains("  test_exec_rootless"),
        "{text}"
    );
    assert!(!text.contains("  test_sanity_rootless"), "{text}");
    // A VMM read as confined while its VM was still starting is not the assertion, except
    // for a test the gate is told fails its boot.
    let (code, text) = Lane {
        asserted: vec![
            ("test_sanity_rootless", "healthy"),
            ("test_exec_rootless", "seen"),
            ("test_boot_fails", "seen"),
        ],
        ..Lane::clean()
    }
    .run();
    assert_eq!(code, 1, "{text}");
    assert!(
        text.contains(unasserted) && text.contains("  test_exec_rootless"),
        "{text}"
    );
    assert!(!text.contains("  test_boot_fails"), "{text}");
    assert_fails(
        Lane {
            boot_fails: "",
            ..Lane::clean()
        },
        "  test_boot_fails",
    );
    // A test that starts no VMM is exempt only by name.
    assert_fails(
        Lane {
            no_vmm: "",
            ..Lane::clean()
        },
        "  vm::tests::trap",
    );
}

#[test]
fn a_failed_or_retried_lane_fails_the_gate() {
    assert_fails(
        Lane {
            exit: 100,
            ..Lane::clean()
        },
        "the lane exited 100",
    );
    assert_fails(
        Lane {
            output: vec!["   TRY 2 PASS [   3.000s] fcvm::bin test_exec_rootless"],
            ..Lane::clean()
        },
        "a test was retried",
    );
    assert_fails(
        Lane {
            output: vec!["       FLAKY 2/2 [   3.000s] fcvm::bin test_exec_rootless"],
            ..Lane::clean()
        },
        "a test was retried",
    );
}

/// A lane whose tests all passed while a VMM hung: a syscall trapped inside Firecracker's
/// logging stops the thread with nothing written and the process still running, so only
/// the API call a test makes before it stops its VM sees it.
#[test]
fn a_vmm_whose_api_went_dead_fails_a_lane_whose_tests_all_passed() {
    let (code, text) = Lane {
        api: vec![
            "ok\ttest_sanity_rootless",
            "dead\ttest_exec_rootless\tthe API of the VMM 4242 did not answer GET /",
        ],
        ..Lane::clean()
    }
    .run();
    assert_eq!(code, 1, "{text}");
    assert!(
        text.contains("a VMM's API stopped answering while its process ran on")
            && text.contains("test_exec_rootless\tthe API of the VMM 4242 did not answer"),
        "{text}"
    );
    // No call recorded at all: nothing says the VMMs were still answering.
    assert_fails(
        Lane {
            api: Vec::new(),
            ..Lane::clean()
        },
        "no VMM's API was called before its test stopped it",
    );
}

/// The call itself: a listener that answers is alive, and one that accepts the connection
/// and says nothing (what a VMM whose API thread hangs looks like), one that is not there,
/// and one that answers something else are not.
#[test]
fn the_api_call_tells_an_answering_vmm_from_a_silent_one() {
    use std::io::{Read, Write};
    use std::time::{Duration, Instant};
    let dir = tempfile::tempdir().unwrap();
    let listen = |name: &str, reply: Option<&'static [u8]>| {
        let path = dir.path().join(name);
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        std::thread::spawn(move || {
            // Connections are kept open, answered or not.
            let mut kept = Vec::new();
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                if let Some(reply) = reply {
                    let mut request = [0u8; 256];
                    let _ = stream.read(&mut request);
                    let _ = stream.write_all(reply);
                }
                kept.push(stream);
            }
        });
        path
    };
    let limit = Duration::from_millis(500);
    let answering = listen(
        "answering.sock",
        Some(b"HTTP/1.1 200 \r\nContent-Length: 2\r\n\r\n{}"),
    );
    assert!(common::vmm_api_answers(&answering, limit));

    let silent = listen("silent.sock", None);
    let started = Instant::now();
    assert!(!common::vmm_api_answers(&silent, limit));
    assert!(
        started.elapsed() >= limit && started.elapsed() < limit * 4,
        "a silent API was given {:?}, not about {limit:?}",
        started.elapsed()
    );

    assert!(!common::vmm_api_answers(
        &dir.path().join("absent.sock"),
        limit
    ));
    let other = listen("other.sock", Some(b"SSH-2.0-not-an-api\r\n"));
    assert!(!common::vmm_api_answers(&other, limit));

    // A process that is not a VMM has no API socket to call, and one that is gone is not
    // hung.
    assert_eq!(common::vmm_api_socket(std::process::id()), None);
    let mut gone = Command::new("true").spawn().unwrap();
    let pid = gone.id();
    gone.wait().unwrap();
    assert_eq!(common::vmm_api_failure(pid), None);
}

/// What the gate cannot check, it does not pass.
#[test]
fn a_lane_the_gate_cannot_read_is_not_clear() {
    let run = |args: &[&str]| {
        let output = Command::new("bash")
            .arg(gate())
            .args(args)
            .output()
            .unwrap();
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    };
    // No summary line: the tests cannot be counted.
    let (code, text) = run(&["1", "echo", "PASS"]);
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("no nextest summary line"), "{text}");
    // Nothing printed, a count that is not a number, no lane.
    for args in [&["1", "true"][..], &["many", "true"], &["1"]] {
        let (code, text) = run(args);
        assert_eq!(code, 2, "{args:?}: {text}");
        assert!(text.contains("confine gate: BLOCKED"), "{args:?}: {text}");
    }
}
