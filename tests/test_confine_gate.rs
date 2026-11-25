//! scripts/confine-gate.sh, the wrapper `make test-confine` runs its lane through: each
//! thing it must fail on, against a stand-in lane that prints what nextest prints. The
//! script needs no VM and no setup, so this runs in the unit lanes.

use std::path::{Path, PathBuf};
use std::process::Command;

/// One run of the gate over a stand-in lane.
struct Lane {
    /// Tests the lane reports as passed.
    passed: Vec<&'static str>,
    /// Of those, the ones whose helpers recorded the confinement assertion.
    asserted: Vec<&'static str>,
    /// Extra lines the lane prints.
    output: Vec<&'static str>,
    /// A line written to a per-test log during the lane, if any log is written at all.
    test_log: Option<&'static str>,
    exit: i32,
    min: usize,
    no_vmm: &'static str,
}

impl Lane {
    fn clean() -> Self {
        let tests = vec![
            "test_sanity_rootless",
            "test_exec_rootless",
            "vm::tests::trap",
        ];
        Lane {
            asserted: tests[..2].to_vec(),
            passed: tests,
            output: Vec::new(),
            test_log: Some("INFO fcvm: VM is healthy"),
            exit: 0,
            min: 3,
            no_vmm: "vm::tests::trap",
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
        for (i, test) in self.passed.iter().enumerate() {
            script.push_str(&format!(
                "echo '        PASS [   1.{i:03}s] ({}/{}) fcvm::bin {test}'\n",
                i + 1,
                self.passed.len()
            ));
        }
        for test in &self.asserted {
            script.push_str(&format!("echo '{test}' >> \"$FCVM_CONFINE_ASSERT_LOG\"\n"));
        }
        for line in &self.output {
            script.push_str(&format!("echo '{line}'\n"));
        }
        if let Some(line) = self.test_log {
            script.push_str(&format!(
                "echo '{line}' > '{}/a-test.log'\n",
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

#[test]
fn a_clean_lane_is_clear_and_runs_confined_enforcing_and_without_retries() {
    let (code, text) = Lane::clean().run();
    assert_eq!(code, 0, "{text}");
    assert!(
        text.contains("retries=0 confine=required seccomp=enforce"),
        "{text}"
    );
    assert!(
        text.contains("confine gate: CLEAR: 3 tests ran once and passed"),
        "{text}"
    );
}

/// A green lane in which a VMM was stopped by the policy: with a retry the test passes on
/// its second try, and a test that expects its VM to fail passes on the first.
#[test]
fn a_policy_stop_fails_a_lane_whose_tests_all_passed() {
    assert_fails(
        Lane {
            test_log: Some(
                "ERROR vm: Firecracker was stopped by its confinement policy: its fc_api \
                 thread made syscall 226",
            ),
            ..Lane::clean()
        },
        "a VMM was stopped by the confinement policy",
    );
    // Firecracker's own line, when fcvm's report did not run.
    assert_fails(
        Lane {
            test_log: Some("Shutting down VM after intercepting a bad syscall (226)."),
            ..Lane::clean()
        },
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

#[test]
fn a_lane_that_ran_fewer_tests_than_the_gate_is_pinned_to_fails() {
    assert_fails(
        Lane {
            min: 4,
            ..Lane::clean()
        },
        "ran 3 tests and the gate is pinned to at least 4",
    );
}

#[test]
fn a_test_that_passed_without_asserting_its_vmm_confined_fails_the_gate() {
    let (code, text) = Lane {
        asserted: vec!["test_sanity_rootless"],
        ..Lane::clean()
    }
    .run();
    assert_eq!(code, 1, "{text}");
    assert!(
        text.contains("passed without asserting their VMM confined")
            && text.contains("  test_exec_rootless"),
        "{text}"
    );
    assert!(!text.contains("  test_sanity_rootless"), "{text}");
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

/// What the gate cannot check, it does not pass.
#[test]
fn a_lane_the_gate_cannot_read_is_not_clear() {
    assert_fails(
        Lane {
            test_log: None,
            ..Lane::clean()
        },
        "no per-test log was written",
    );
    let dir = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        let output = Command::new("bash")
            .arg(gate())
            .args(args)
            .env("CONFINE_GATE_LOG_DIR", dir.path())
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
