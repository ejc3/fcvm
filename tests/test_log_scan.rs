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

/// The summary's flaky count must be read from the form nextest actually emits.
///
/// Measured against 59 cached CI job logs: the parenthetical is never `(N flaky)`
/// alone, it is `(1 slow, 1 flaky)`. A pattern anchored on `\(([0-9]+) flaky\)`
/// therefore matched nothing in production and the flaky gate never once fired,
/// while reporting `verdict: CLEAN`, exit 0, on a log whose own summary said
/// otherwise. Job 93992054596 is exactly that log.
#[test]
fn the_summary_flaky_count_is_read_when_other_items_share_the_parens() {
    let log = repo_root().join("tests/fixtures/nextest-flaky-with-slow.log");
    let (out, code) = run_scan(&log);

    assert!(
        out.contains("verdict: FLAKY"),
        "a summary reading `(1 slow, 1 flaky)` must be reported FLAKY. If this says \
         CLEAN, the flaky pattern requires `flaky` to be the only item in the parens \
         and cannot match a real nextest summary.\n{out}"
    );
    assert_eq!(code, 1, "a flaky run must exit non-zero.\n{out}");
}

/// Every summary in the log must be judged, not only the last one.
///
/// The SnapshotEnabled matrix runs the suite twice in one job, so a job log
/// carries two summary lines. In 7 of the 19 cached multi-summary jobs the flaky
/// summary is the FIRST one, and `tail -1` discards it.
#[test]
fn a_flaky_summary_is_not_discarded_by_a_later_clean_one() {
    let log = repo_root().join("tests/fixtures/nextest-two-summaries-flaky-first.log");
    let (out, code) = run_scan(&log);

    assert!(
        out.contains("verdict: FLAKY"),
        "the FIRST summary reports 2 flaky and must not be dropped in favour of the \
         last. A scanner that only reads the final summary is blind to the first \
         suite run in every SnapshotEnabled job.\n{out}"
    );
    assert_eq!(code, 1, "a flaky run must exit non-zero.\n{out}");
}

/// A retry counts however its first attempt died, not only when it said FAIL.
///
/// nextest reports a slow-timeout kill as `TRY 1 TMT`. Five such retries occurred
/// in the last 6 days of CI (900s hang, then a 7s pass), every one inside a job
/// that reported green. A pattern matching only `TRY n FAIL` cannot see them.
#[test]
fn a_retry_after_a_timeout_kill_is_counted() {
    let log = repo_root().join("tests/fixtures/nextest-retry-timeout.log");
    let (out, code) = run_scan(&log);

    assert_eq!(
        field(&out, "try_fail"),
        1,
        "`TRY 1 TMT` is a retried failure and must be counted. A 900s hang that \
         passes on retry is a hang bug, not contention, and this is the only place \
         it is visible.\n{out}"
    );
    assert_eq!(
        code, 1,
        "a run containing a retried timeout must not be reported clean.\n{out}"
    );
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

/// Fail with ONE clear message when the gate's own dependency is missing, instead of
/// six assertion failures whose real cause ("jq: command not found") is buried in the
/// captured output. This bit for real: `jq` was absent from the CI container, and the
/// first symptom was six unrelated-looking test failures.
fn require_jq() {
    let ok = Command::new("sh")
        .args(["-c", "command -v jq"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(
        ok,
        "`jq` is not installed, and the review-thread gate cannot run without it. \
         This is a MISSING DEPENDENCY, not a broken gate — install jq (it is in the \
         Containerfile for exactly this reason). The gate itself refusing to render a \
         verdict without jq is covered by `the_gate_blocks_when_it_cannot_run_at_all`."
    );
}

/// Every review-thread fixture carries the PR-level fields the gate reads: the head
/// commit, a check suite dating its arrival, and a stranger's APPROVED review of that
/// head. Without them the gate blocks on an unreviewed head — correctly — and each of
/// these tests would then pass for the wrong reason, testing coverage instead of the
/// rule it is named for. An APPROVED review is not a finding, so it adds nothing to
/// answer; it only says the head was looked at.
fn run_threads(fixture: &str) -> (String, i32) {
    require_jq();
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
/// "Fully resolved" now means resolved AND dispositioned — the fixture's
/// thread carries a NOT-A-DEFECT reply, since resolution alone stopped
/// being sufficient when every resolved thread began requiring an answer.
#[test]
fn a_fully_resolved_pr_passes_the_gate() {
    let (out, code) = run_threads("review-threads-clear.json");
    assert_eq!(
        code, 0,
        "an all-resolved, dispositioned PR must pass:\n{out}"
    );
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
    // The gate names the missing thing rather than guessing whether the
    // finding was defect-shaped: the regex that used to decide that failed
    // both ways (AGENTS.md), so an undispositioned resolved thread blocks
    // whatever it says.
    assert!(out.contains("carry no disposition reply"), "{out}");
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

/// A gate that cannot run must BLOCK, not pass.
///
/// This is a regression test for the gate itself. `jq` is absent from the CI container,
/// so every `jq` call errored to stderr, the counts came back empty, and the script
/// printed `verdict: CLEAR` and exited 0 — waving every PR through precisely because it
/// could not evaluate any of them. Strictly worse than no gate, because it looks like one.
///
/// Runs the script with a PATH that genuinely cannot reach `jq`. Note `/bin` is a symlink
/// to `/usr/bin` on usr-merged systems, so removing one directory from PATH does NOT hide
/// a tool — the PATH has to be replaced outright.
#[test]
fn the_gate_blocks_when_it_cannot_run_at_all() {
    let empty = std::env::temp_dir().join(format!("fcvm-nodeps-{}", std::process::id()));
    std::fs::create_dir_all(&empty).unwrap();

    let out = Command::new("/usr/bin/bash")
        .arg(repo_root().join("scripts/check-review-threads.sh"))
        .arg("--from-file")
        .arg(repo_root().join("tests/fixtures/review-threads-unresolved.json"))
        .env("PATH", &empty) // jq unreachable
        .output()
        .expect("script must be runnable");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let code = out.status.code().unwrap_or(-1);

    assert_ne!(
        code, 0,
        "with `jq` unreachable the gate MUST NOT exit 0. Exit 0 here means it reported a \
         verdict it had no ability to compute.\n{combined}"
    );
    assert!(
        !combined.contains("verdict: CLEAR"),
        "the gate claimed CLEAR while unable to parse anything — fail closed, always.\n{combined}"
    );
    assert!(
        combined.contains("BLOCKED"),
        "a gate that cannot evaluate must say so explicitly.\n{combined}"
    );
    let _ = std::fs::remove_dir_all(&empty);
}

/// The gate must work on REAL data, not only on fixtures someone hand-wrote.
///
/// This exists because a hand-written fixture lied. It carried two comments per thread
/// while the query asked for `comments(first: 1)`, so the "a RED-VERIFIED reply satisfies
/// the gate" test passed against a response the code could never produce — green, and
/// proving nothing. `scripts/capture-review-threads-fixture.sh` now dumps what the gate
/// itself assembles (`--dump-payload`), so a fixture cannot drift from the query, and
/// cannot omit a field the gate later starts reading — which is how the previous capture,
/// taken before the gate read the head at all, ended up blocking every test that used it.
///
/// Captured from PR #867 on 2026-08-28 at 22:43Z: head 9ebed542, 18 threads with 3
/// unresolved, 44 reviews, 17 top-level comments. It replaces a capture of PR #748, whose
/// threads have all been resolved since, leaving nothing unresolved to parse.
#[test]
fn the_gate_handles_a_real_captured_pr_response() {
    let (out, code) = run_threads("review-threads-live-867.json");

    assert_ne!(code, 0, "a PR with unresolved threads must block:\n{out}");
    assert!(
        out.contains("18 total, 3 unresolved"),
        "the captured response has 18 threads with 3 unresolved; if this count moved, \
         re-capture with scripts/capture-review-threads-fixture.sh and check WHY.\n{out}"
    );
    // Real bodies are markdown with badges, HTML comments and embedded shell blocks —
    // far messier than anything hand-written. Parsing must survive that.
    assert!(
        out.contains("UNRESOLVED"),
        "individual findings must still be listed from real, messy bodies.\n{out}"
    );
}

/// Every fixture must carry the fields the live query actually selects. A fixture missing
/// one would make the gate silently skip a check it believes it performed.
#[test]
fn every_fixture_matches_the_shape_the_query_returns() {
    const REQUIRED: [&str; 5] = ["author", "body", "line", "originalLine", "path"];
    // The gate reads these too, and a fixture without them is judged under a rule it
    // cannot satisfy: no head means "this gate cannot tell whether the code being merged
    // was reviewed", which is a BLOCK, so every test using such a fixture fails for a
    // reason that has nothing to do with what it asserts.
    const REQUIRED_PR_LEVEL: [&str; 4] = ["headRefOid", "checkSuites", "reviews", "comments"];
    let dir = repo_root().join("tests/fixtures");
    let mut checked = 0;

    for entry in std::fs::read_dir(&dir).expect("fixtures dir") {
        let path = entry.expect("dir entry").path();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if !name.starts_with("review-threads-") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("read fixture");
        for field in REQUIRED {
            assert!(
                text.contains(&format!("\"{field}\"")),
                "fixture {name} is missing the `{field}` field that the live query selects. \
                 A fixture that does not match the query lets a test pass against data the \
                 code can never receive."
            );
        }
        for field in REQUIRED_PR_LEVEL {
            assert!(
                text.contains(&format!("\"{field}\"")),
                "fixture {name} is missing the `{field}` field the gate reads at PR level. \
                 Regenerate it with scripts/capture-review-threads-fixture.sh, or add the \
                 field by hand; a fixture the gate cannot judge blocks on its own shape."
            );
        }
        assert!(
            text.contains("\"isResolved\""),
            "fixture {name} lacks isResolved — the only field that means 'resolved'."
        );
        checked += 1;
    }
    assert!(
        checked >= 4,
        "expected several review-thread fixtures, found {checked}"
    );
}

/// A defect report cannot prove itself.
///
/// The marker must appear in a REPLY. If the check searched every comment including the
/// opening one, a bot report that merely QUOTES the policy — "resolutions require a
/// RED-VERIFIED: <test> reply" — would satisfy its own requirement and close a real bug
/// on nothing. The fixture is exactly that shape: one comment, containing the marker.
#[test]
fn a_defect_report_cannot_serve_as_its_own_red_verification() {
    let (out, code) = run_threads("review-threads-selfproof.json");
    assert_ne!(
        code, 0,
        "a lone defect comment containing the marker must NOT count as proof — the \
         evidence has to come from a reply.\n{out}"
    );
    assert!(out.contains("carry no disposition reply"), "{out}");
}

/// The run's OWN summary is authoritative — counting per-test lines is not enough.
///
/// A truncated or filtered log can carry the summary while every `FAIL [` line is gone.
/// This scanner then counted zero failures and said CLEAN, exit 0, on a run the summary
/// itself called failed:
///   "Summary [10.0s] 1 tests run: 0 passed, 1 failed"  ->  verdict: CLEAN
/// which is precisely the defect it exists to catch.
#[test]
fn a_summary_reporting_failures_is_never_clean() {
    let dir = std::env::temp_dir().join(format!("fcvm-sumfail-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("failing-summary.log");
    std::fs::write(
        &log,
        "     Summary [  10.0s] 1 tests run: 0 passed, 1 failed, 0 skipped\n",
    )
    .unwrap();

    let (out, code) = run_scan(&log);
    assert_eq!(
        code, 1,
        "a summary reporting failures must exit 1, not 0.\n{out}"
    );
    assert!(out.contains("verdict: FAILED"), "{out}");
    assert!(
        !out.contains("CLEAN"),
        "the scanner must not call this run clean.\n{out}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Tests that vanish between "run" and "passed" are not a pass either.
#[test]
fn a_summary_with_unaccounted_tests_is_not_clean() {
    let dir = std::env::temp_dir().join(format!("fcvm-sumgap-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("unaccounted.log");
    // 5 run, 3 passed, 0 failed: two tests are simply missing.
    std::fs::write(
        &log,
        "     Summary [ 1.0s] 5 tests run: 3 passed, 0 skipped\n",
    )
    .unwrap();

    let (out, code) = run_scan(&log);
    assert_ne!(
        code, 0,
        "3-of-5 passed with 0 failed must not be clean.\n{out}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The retry pattern must anchor on the verdict form, not bare prose.
#[test]
fn retry_matching_does_not_fire_on_prose() {
    let dir = std::env::temp_dir().join(format!("fcvm-prose-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("prose.log");
    // A test that PRINTS about retries must not be counted as one.
    std::fs::write(
        &log,
        "    note: the harness will TRY 1 FAIL semantics on the next pass\n\
             Summary [ 1.0s] 1 tests run: 1 passed, 0 skipped\n",
    )
    .unwrap();

    let (out, code) = run_scan(&log);
    assert_eq!(
        field(&out, "try_fail"),
        0,
        "prose mentioning a retry is not a retry; only `TRY n FAIL [` counts.\n{out}"
    );
    assert_eq!(code, 0, "a genuinely clean run must stay clean.\n{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Malformed thread data must BLOCK, exactly like a missing dependency.
///
/// `jq` yields `null` for a missing path and an empty string on a parse error, and
/// `[ "" -gt 0 ]` is a shell error rather than a block — so bad input previously slid
/// through to CLEAR.
#[test]
fn malformed_thread_data_blocks_instead_of_clearing() {
    let dir = std::env::temp_dir().join(format!("fcvm-malformed-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("malformed.json");
    // A thread with no isResolved: the one field that means "resolved".
    std::fs::write(
        &f,
        r#"{"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[{"isOutdated":true}]}}}}}"#,
    )
    .unwrap();

    let out = Command::new("bash")
        .arg(repo_root().join("scripts/check-review-threads.sh"))
        .arg("--from-file")
        .arg(&f)
        .output()
        .expect("script must run");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_ne!(
        out.status.code().unwrap_or(-1),
        0,
        "unparseable thread data must not produce a passing verdict.\n{combined}"
    );
    assert!(combined.contains("BLOCKED"), "{combined}");
    let _ = std::fs::remove_dir_all(&dir);
}
