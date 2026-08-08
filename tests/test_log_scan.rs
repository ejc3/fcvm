//! Executable guards for the log-scanning and review-gate footguns.
//!
//! Each of these pins a mistake that has actually shipped and reported green.
//! They are written to go RED if the underlying pattern regresses — a doc that
//! says "don't use `grep '^ *FAIL'`" cannot fire; these can.
//!
//! Every assertion here was confirmed to fail with the corresponding fix
//! reverted (see the PR body for the observed failure output).

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn run_scan(log: &Path) -> (String, i32) {
    let out = Command::new("bash")
        .arg(repo_root().join("scripts/scan-test-log.sh"))
        .arg(log)
        .output()
        .expect("scan-test-log.sh must be runnable");
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    (s, out.status.code().unwrap_or(-1))
}

fn field(out: &str, key: &str) -> i64 {
    out.lines()
        .find_map(|l| l.strip_prefix(&format!("{key}: ")))
        .unwrap_or_else(|| panic!("scan output has no `{key}:` field:\n{out}"))
        .trim()
        .parse()
        .unwrap_or_else(|e| panic!("`{key}` is not a number ({e}):\n{out}"))
}

/// THE trap: nextest writes a retry as `TRY 1 FAIL`, so `grep '^ *FAIL'` matches
/// none of them. A test that failed and then passed on retry disappears into a
/// green total — which is precisely how a flake survives review.
#[test]
fn a_retry_is_counted_as_a_retry_and_not_lost() {
    let log = repo_root().join("tests/fixtures/nextest-flaky.log");
    let (out, code) = run_scan(&log);

    assert_eq!(
        field(&out, "try_fail"),
        1,
        "the `TRY 1 FAIL` line must be counted. If this is 0, the pattern has \
         regressed to something that cannot see retries, and every flake will \
         now report as a clean pass.\n{out}"
    );
    // Assert the SPECIFIC verdict, not merely "nonzero". An earlier version used
    // `assert_ne!(code, 0)` and passed for the wrong reason: dropping the literal
    // `^[` ANSI strip made the summary line unparseable, so the verdict silently
    // became UNKNOWN (exit 3) instead of FLAKY (exit 1) — still nonzero, still
    // "passing", while the scanner had in fact stopped understanding the log.
    assert_eq!(
        code, 1,
        "a run containing a retry must be reported FLAKY (exit 1), not clean and \
         not UNKNOWN. Exit 3 here means the summary line was not parsed — check \
         that BOTH ANSI encodings are still being stripped.\n{out}"
    );
    assert!(
        out.contains("verdict: FLAKY"),
        "the verdict must name the flake explicitly.\n{out}"
    );
    // The fixture's summary is wrapped in the literal `^[` ANSI form produced by
    // `gh run view --log`. If stripping regresses to real-ESC only, this line
    // stops matching and the whole scan degrades to UNKNOWN.
    assert!(
        out.contains("681 tests run"),
        "the summary must be parsed out of literal `^[`-encoded ANSI.\n{out}"
    );
}

/// Test bodies print their own `PASSED:` lines. Counting `grep -c PASS` mixes
/// those with nextest's verdicts — measured once at 136 against a real 119.
#[test]
fn test_internal_passed_lines_do_not_inflate_the_verdict_count() {
    let log = repo_root().join("tests/fixtures/nextest-flaky.log");
    let (out, _) = run_scan(&log);

    // The fixture has exactly 3 `PASS [` verdicts and 4 test-internal
    // "PASSED:"/"PASSED!" lines that must not be counted.
    assert_eq!(
        field(&out, "pass"),
        3,
        "only `PASS [` verdicts count. A higher number means the pattern is \
         also matching the `PASSED:` lines test bodies print themselves.\n{out}"
    );
}

/// `TRY 1 FAIL [` also ends in ` FAIL [`, so a naive hard-failure pattern
/// double-reports every retry as a hard failure.
#[test]
fn a_retry_is_not_also_counted_as_a_hard_failure() {
    let log = repo_root().join("tests/fixtures/nextest-flaky.log");
    let (out, _) = run_scan(&log);

    assert_eq!(
        field(&out, "fail"),
        0,
        "the fixture contains no hard `FAIL [` verdict — only a retry. A nonzero \
         count means `TRY n FAIL` is being double-counted as a hard failure.\n{out}"
    );
}

/// Logs arrive with real ESC bytes locally and the literal two-character `^[`
/// form from `gh run view --log`. Both must be stripped or every pattern
/// silently misses.
#[test]
fn both_ansi_encodings_are_stripped() {
    let dir = std::env::temp_dir().join(format!("fcvm-logscan-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("real-esc.log");
    // Real ESC (0x1b) rather than the caret form used in the checked-in fixture.
    let body = format!(
        "{esc}[35;1m  TRY 1 FAIL{esc}[0m [   2.9s] (--) fcvm::t test_x\n\
         {esc}[32;1m     Summary{esc}[0m [ 10.0s] 1 tests run: 1 passed, 0 skipped\n",
        esc = '\u{1b}'
    );
    std::fs::write(&log, body).unwrap();

    let (out, _) = run_scan(&log);
    assert_eq!(
        field(&out, "try_fail"),
        1,
        "a retry wrapped in real ESC sequences must still be seen.\n{out}"
    );
    assert!(
        out.contains("1 tests run"),
        "the summary line must survive ANSI stripping.\n{out}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A run with no summary line is UNKNOWN, never CLEAN. GitHub will not serve
/// job logs until the whole run completes, so a truncated log is a normal thing
/// to be handed — and reporting it clean is how a half-finished run gets merged.
#[test]
fn a_log_without_a_summary_is_never_reported_clean() {
    let dir = std::env::temp_dir().join(format!("fcvm-logscan-nosum-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("truncated.log");
    std::fs::write(&log, "        PASS [   0.01s] (1/2) fcvm::t test_a\n").unwrap();

    let (out, code) = run_scan(&log);
    assert_ne!(code, 0, "a truncated log must not exit 0:\n{out}");
    assert!(
        out.contains("UNKNOWN"),
        "a missing summary must be reported as UNKNOWN, not CLEAN.\n{out}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Review-thread gate
// ---------------------------------------------------------------------------

fn run_threads(fixture: &str) -> (String, i32) {
    let out = Command::new("bash")
        .arg(repo_root().join("scripts/check-review-threads.sh"))
        .arg("--from-file")
        .arg(repo_root().join("tests/fixtures").join(fixture))
        .output()
        .expect("check-review-threads.sh must be runnable");
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    (s, out.status.code().unwrap_or(-1))
}

/// A PR can sit at all-green CI, MERGEABLE, and still carry unanswered findings.
/// Observed: 15 green checks with 19 unresolved threads. CI state says nothing
/// about whether a review was answered.
#[test]
fn unresolved_review_threads_block_the_merge() {
    let (out, code) = run_threads("review-threads-unresolved.json");
    assert_ne!(code, 0, "unresolved threads must block:\n{out}");
    assert!(
        out.contains("2 unresolved"),
        "both unresolved threads must be reported, including the outdated one — \
         `isOutdated` means the diff moved, not that the finding was handled.\n{out}"
    );
}

/// The counterpart: the gate must be able to pass, or it is just a wall.
#[test]
fn a_fully_resolved_pr_passes_the_gate() {
    let (out, code) = run_threads("review-threads-clear.json");
    assert_eq!(code, 0, "an all-resolved PR must pass:\n{out}");
    assert!(out.contains("CLEAR"), "{out}");
}

/// Resolving a "this is broken" finding requires citing a test that was watched
/// failing. Closing it on the same judgement that shipped the bug is not proof.
#[test]
fn a_resolved_defect_claim_without_a_red_test_blocks_the_merge() {
    let (out, code) = run_threads("review-threads-resolved-unproven.json");
    assert_ne!(
        code, 0,
        "a resolved thread describing a panic, with no RED-VERIFIED reply, must \
         block — otherwise a defect is closed by assertion.\n{out}"
    );
    assert!(out.contains("UNPROVEN"), "{out}");
}

/// ...and citing the test clears it, so the rule is satisfiable.
#[test]
fn a_resolved_defect_claim_with_a_red_test_is_accepted() {
    let (out, code) = run_threads("review-threads-resolved-proven.json");
    assert_eq!(
        code, 0,
        "a RED-VERIFIED reply must satisfy the gate:\n{out}"
    );
}
