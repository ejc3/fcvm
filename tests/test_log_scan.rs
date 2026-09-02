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
///
/// The check reads the payload rather than searching its text. A substring search cannot
/// say WHERE a name appears, and every PR-level name also occurs inside a thread comment,
/// so `"comments"` is found in a payload that carries no PR-level comments connection at
/// all. `the_fixture_shape_check_reads_the_payload_not_the_text` pins that.
fn fixture_shape_problem(name: &str, text: &str) -> Option<String> {
    let hint = "Regenerate it with scripts/capture-review-threads-fixture.sh, or add the \
                field by hand; a fixture that does not match the query lets a test pass \
                against data the code can never receive.";
    let doc: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => return Some(format!("fixture {name} is not JSON: {e}")),
    };
    let pr = &doc["data"]["repository"]["pullRequest"];
    if !pr.is_object() {
        return Some(format!(
            "fixture {name} has no .data.repository.pullRequest object. {hint}"
        ));
    }
    // The head, and the check suites that date its arrival. Without them the gate answers
    // "this gate cannot tell whether the code being merged was reviewed", which is a BLOCK,
    // so every test using such a fixture fails for a reason unrelated to what it asserts.
    if !pr["headRefOid"].is_string() {
        return Some(format!(
            "fixture {name}: .data.repository.pullRequest.headRefOid is not a string. {hint}"
        ));
    }
    let suites = &pr["commits"]["nodes"][0]["commit"]["checkSuites"]["nodes"];
    if !suites.is_array() {
        return Some(format!(
            "fixture {name}: .commits.nodes[0].commit.checkSuites.nodes is not an array. {hint}"
        ));
    }
    // The three connections a claim can arrive in. Each is its own PR-level field: a
    // thread's comments connection is not the PR's.
    for field in ["reviews", "comments", "reviewThreads"] {
        if !pr[field]["nodes"].is_array() {
            return Some(format!(
                "fixture {name}: .data.repository.pullRequest.{field}.nodes is not an \
                 array. {hint}"
            ));
        }
    }
    for (i, thread) in pr["reviewThreads"]["nodes"]
        .as_array()
        .expect("checked above")
        .iter()
        .enumerate()
    {
        if !thread["isResolved"].is_boolean() {
            return Some(format!(
                "fixture {name}: reviewThreads.nodes[{i}].isResolved is not a boolean, \
                 and it is the only field that means 'resolved'. {hint}"
            ));
        }
        // The other two fields the query selects on a thread. `id` is what the gate pages an
        // oversized thread's comments by, and what orders the two reads it compares;
        // `isOutdated` is the flag the unresolved rule ignores on purpose, and a fixture
        // without it cannot show that an outdated finding still counts.
        if !thread["id"].is_string() {
            return Some(format!(
                "fixture {name}: reviewThreads.nodes[{i}].id is not a string. {hint}"
            ));
        }
        if !thread["isOutdated"].is_boolean() {
            return Some(format!(
                "fixture {name}: reviewThreads.nodes[{i}].isOutdated is not a boolean. {hint}"
            ));
        }
        let Some(comments) = thread["comments"]["nodes"].as_array() else {
            return Some(format!(
                "fixture {name}: reviewThreads.nodes[{i}].comments.nodes is not an array. \
                 {hint}"
            ));
        };
        for (j, comment) in comments.iter().enumerate() {
            let at = format!("reviewThreads.nodes[{i}].comments.nodes[{j}]");
            for (path, value) in [
                ("author.login", &comment["author"]["login"]),
                ("body", &comment["body"]),
                ("path", &comment["path"]),
            ] {
                if !value.is_string() {
                    return Some(format!(
                        "fixture {name}: {at}.{path} is not a string. {hint}"
                    ));
                }
            }
            // Both are selected and either may be null; the gate falls back from one to
            // the other, so a fixture that omits either is answering a question the live
            // query does not.
            for key in ["line", "originalLine"] {
                if comment.get(key).is_none() {
                    return Some(format!("fixture {name}: {at} has no `{key}` key. {hint}"));
                }
            }
        }
    }
    None
}

/// A payload whose only `comments` key is a thread's must not pass the PR-level check.
#[test]
fn the_fixture_shape_check_reads_the_payload_not_the_text() {
    // This payload carries every PR-level field but `comments`, and the only `"comments"`
    // in its text is the thread's own connection. A substring search finds that one and
    // reports the fixture complete.
    let no_pr_comments = r#"{"data":{"repository":{"pullRequest":{
        "author":{"login":"me"},
        "headRefOid":"deadbeef",
        "commits":{"nodes":[{"commit":{"committedDate":"2026-01-02T00:00:00Z",
          "checkSuites":{"nodes":[{"createdAt":"2026-01-02T00:30:00Z"}]}}}]},
        "reviews":{"nodes":[]},
        "reviewThreads":{"nodes":[{"id":"PRRT_inline_1","isResolved":false,"isOutdated":false,
          "comments":{"nodes":[
          {"author":{"login":"reviewer"},"path":"a.rs","line":1,"originalLine":1,
           "body":"P1: this drops the last row"}]}}]}
      }}}}"#;
    assert!(
        fixture_shape_problem("no-pr-comments", no_pr_comments).is_some(),
        "a payload with no PR-level comments connection must be rejected; finding the \
         word inside a thread's own comments connection is not finding the field."
    );

    // The same payload with the PR-level connections really present is accepted, so the
    // rejection above is about the missing fields and not about the shape check refusing
    // everything.
    let complete = r#"{"data":{"repository":{"pullRequest":{
        "author":{"login":"me"},
        "headRefOid":"deadbeef",
        "commits":{"nodes":[{"commit":{"committedDate":"2026-01-02T00:00:00Z",
          "checkSuites":{"nodes":[{"createdAt":"2026-01-02T00:30:00Z"}]}}}]},
        "reviews":{"nodes":[]},
        "comments":{"nodes":[]},
        "reviewThreads":{"nodes":[{"id":"PRRT_inline_2","isResolved":false,"isOutdated":false,
          "comments":{"nodes":[
          {"author":{"login":"reviewer"},"path":"a.rs","line":1,"originalLine":1,
           "body":"P1: this drops the last row"}]}}]}
      }}}}"#;
    assert_eq!(
        fixture_shape_problem("complete", complete),
        None,
        "a payload carrying every field the query selects must be accepted"
    );
}

/// A thread comes back from the query with an `id` and an `isOutdated` flag, so a fixture
/// without them is a shape GitHub cannot return.
///
/// The validator checked `isResolved` alone. `id` is what the gate keys an oversized
/// thread's comment paging on, and what orders the two reads it compares; `isOutdated` is
/// the flag the unresolved rule deliberately ignores, and a fixture that omits it cannot
/// show that it does. Five fixtures were missing the id, and so was the payload the test
/// above accepts as complete: a check that certifies a shape the live query never produces.
#[test]
fn a_fixture_thread_without_an_id_or_an_outdated_flag_is_rejected() {
    let payload = |thread_fields: &str| {
        format!(
            r#"{{"data":{{"repository":{{"pullRequest":{{
        "author":{{"login":"me"}},
        "headRefOid":"deadbeef",
        "commits":{{"nodes":[{{"commit":{{"committedDate":"2026-01-02T00:00:00Z",
          "checkSuites":{{"nodes":[{{"createdAt":"2026-01-02T00:30:00Z"}}]}}}}}}]}},
        "reviews":{{"nodes":[]}},
        "comments":{{"nodes":[]}},
        "reviewThreads":{{"nodes":[{{{thread_fields},"comments":{{"nodes":[
          {{"author":{{"login":"reviewer"}},"path":"a.rs","line":1,"originalLine":1,
           "body":"P1: this drops the last row"}}]}}}}]}}
      }}}}}}}}"#
        )
    };

    assert!(
        fixture_shape_problem(
            "no-id",
            &payload(r#""isResolved":false,"isOutdated":false"#)
        )
        .is_some(),
        "a thread with no `id` must be rejected: the query selects it, and the gate pages \
         an oversized thread's comments by it."
    );
    assert!(
        fixture_shape_problem(
            "id-not-a-string",
            &payload(r#""id":7,"isResolved":false,"isOutdated":false"#)
        )
        .is_some(),
        "an `id` that is not a string is not the id the query returns."
    );
    assert!(
        fixture_shape_problem(
            "no-outdated",
            &payload(r#""id":"PRRT_x","isResolved":false"#)
        )
        .is_some(),
        "a thread with no `isOutdated` must be rejected: the query selects it, and a \
         fixture that omits it cannot show that an outdated finding still counts."
    );
    assert!(
        fixture_shape_problem(
            "outdated-not-a-bool",
            &payload(r#""id":"PRRT_x","isResolved":false,"isOutdated":"true""#)
        )
        .is_some(),
        "the string \"true\" is not the boolean the query returns."
    );
    assert_eq!(
        fixture_shape_problem(
            "complete",
            &payload(r#""id":"PRRT_x","isResolved":false,"isOutdated":false"#)
        ),
        None,
        "a thread carrying every field the query selects must be accepted"
    );
}

#[test]
fn every_fixture_matches_the_shape_the_query_returns() {
    let dir = repo_root().join("tests/fixtures");
    let mut checked = 0;

    for entry in std::fs::read_dir(&dir).expect("fixtures dir") {
        let path = entry.expect("dir entry").path();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if !name.starts_with("review-threads-") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("read fixture");
        assert_eq!(fixture_shape_problem(&name, &text), None);
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

/// The answer this gate asks for must not become a new question.
///
/// A PR-level review body from the PR author was claimable like anyone else's, so the review
/// the gate tells you to post ("Post a review on the PR OPENING with one of RED-VERIFIED: /
/// NOT-A-DEFECT: / DISAGREE:") became a fresh unanswered claim whenever it opened with
/// anything else, and the count grew by one every round: #874 carried 4 such bodies and #872
/// 5, all the author's own acks, burying the bot findings that really had no answer. A review
/// from the author is an answer to this PR, not a finding against it, and the head-coverage
/// rule already discounts the author for the same reason. Everyone else stays claimable, and
/// the three-token vocabulary is unchanged: REVIEW-ACK answers nothing, it just stops being a
/// claim itself.
///
/// The shell harness (scripts/test-check-review-threads.sh, "finding 41") carries the full
/// matrix, including the author's top-level COMMENT staying claimable; this one runs in CI.
#[test]
fn an_authors_review_body_is_an_answer_not_a_new_claim() {
    require_jq();
    let payload = |ack_author: &str| {
        format!(
            r#"{{"data":{{"repository":{{"pullRequest":{{
        "author":{{"login":"me"}},
        "headRefOid":"deadbeef",
        "commits":{{"nodes":[{{"commit":{{"committedDate":"2026-01-02T00:00:00Z",
          "checkSuites":{{"nodes":[{{"createdAt":"2026-01-02T00:30:00Z"}}]}}}}}}]}},
        "reviewThreads":{{"nodes":[]}},
        "comments":{{"nodes":[]}},
        "reviews":{{"nodes":[
          {{"author":{{"login":"reviewer"}},"state":"APPROVED","submittedAt":"2026-01-02T01:00:00Z",
           "body":"","commit":{{"oid":"deadbeef"}}}},
          {{"author":{{"login":"{ack_author}"}},"state":"COMMENTED",
           "submittedAt":"2026-01-03T00:00:00Z",
           "body":"REVIEW-ACK: round 3, three findings closed in abc1234"}}]}}
      }}}}}}}}"#
        )
    };

    let dir = std::env::temp_dir().join(format!("fcvm-gate-ack-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let run = |name: &str, body: String| {
        let f = dir.join(name);
        std::fs::write(&f, body).unwrap();
        let out = Command::new("bash")
            .arg(repo_root().join("scripts/check-review-threads.sh"))
            .arg("--from-file")
            .arg(&f)
            .output()
            .expect("check-review-threads.sh must be runnable");
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (combined, out.status.code().unwrap_or(-1))
    };

    let (out, code) = run("author-ack.json", payload("me"));
    assert_eq!(
        code, 0,
        "the PR author's own review body is an answer, not a claim. Counting it as one \
         makes every acknowledgement create the obligation it was posted to discharge.\n{out}"
    );
    assert!(out.contains("CLEAR"), "{out}");

    let (out, code) = run("stranger-ack.json", payload("reviewer"));
    assert_eq!(
        code, 1,
        "the same body from anyone else is a PR-level statement that still needs one of \
         RED-VERIFIED / NOT-A-DEFECT / DISAGREE.\n{out}"
    );
    assert!(out.contains("carry no disposition"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A timestamp comparison must be between INSTANTS, not between strings.
///
/// The gate's timestamps do not all arrive in one shape. Check-suite and comment
/// timestamps come back whole-second ("2026-01-02T00:30:00Z"); the datetime Codex writes
/// into a review-summary row carries a fraction ("2026-01-02T00:30:00.123456Z"). Compared
/// as strings, "." sorts before "Z", so a result recorded in the same second as the head's
/// check suite reads as EARLIER than the head arrived, and the head is reported unreviewed
/// for as long as that result is its only coverage. String order also ranks anything that
/// is not a timestamp: "a while ago" sorts after every digit, so an unparsable date used to
/// postdate everything and grant coverage. A date the gate cannot parse orders against
/// nothing and must block.
///
/// The shell harness (scripts/test-check-review-threads.sh, "finding 32") carries the full
/// matrix; these two run in CI.
#[test]
fn review_gate_orders_timestamps_as_instants_not_as_strings() {
    require_jq();
    // Codex's no-findings comment as posted: the phrase, the reviewed commit, and the
    // folded About Codex block.
    const VERDICT: &str = r#""Codex Review: Didn't find any major issues. Bravo.\n\n**Reviewed commit:** `deadbeef`\n\n<details> <summary>ℹ️ About Codex in GitHub</summary>\n<br/>\n\n[Your team has set up Codex to review pull requests in this repo](https://chatgpt.com/codex/cloud/settings/general). Reviews are triggered when you\n- Open a pull request for review\n- Mark a draft as ready\n- Comment \"@codex review\".\n\nIf Codex has suggestions, it will comment; otherwise it will react with 👍.\n\nCodex can also answer questions or update the PR. Try commenting \"@codex address that feedback\".\n</details>""#;
    let payload = |suite: &str, posted: &str| {
        let comments = format!(
            r#"[{{"author":{{"login":"chatgpt-codex-connector","__typename":"Bot"}},"createdAt":"{posted}","updatedAt":"{posted}","body":{VERDICT}}}]"#
        );
        format!(
            r#"{{"data":{{"repository":{{"pullRequest":{{"author":{{"login":"me"}},"headRefOid":"deadbeef","commits":{{"nodes":[{{"commit":{{"committedDate":"2026-01-02T00:00:00Z","checkSuites":{{"nodes":[{{"createdAt":"{suite}"}}]}}}}}}]}},"reviewThreads":{{"nodes":[]}},"reviews":{{"nodes":[]}},"comments":{{"nodes":{comments}}},"recheck":{{"comments":{{"nodes":{comments}}}}}}}}}}}}}"#
        )
    };

    let dir = std::env::temp_dir().join(format!("fcvm-gate-ts-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let run = |name: &str, body: String| {
        let f = dir.join(name);
        std::fs::write(&f, body).unwrap();
        let out = Command::new("bash")
            .arg(repo_root().join("scripts/check-review-threads.sh"))
            .arg("--from-file")
            .arg(&f)
            .output()
            .expect("check-review-threads.sh must be runnable");
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (combined, out.status.code().unwrap_or(-1))
    };

    // The verdict lands 100ms after the head's check suite, so it covers the head.
    let (out, code) = run(
        "same-second.json",
        payload("2026-01-02T00:30:00Z", "2026-01-02T00:30:00.100Z"),
    );
    assert_eq!(
        code, 0,
        "a result recorded 100ms after the head arrived covers it; comparing the two as \
         strings puts the fractional one first and leaves the head uncovered forever.\n{out}"
    );
    assert!(out.contains("HEAD COVERED"), "{out}");

    // An arrival the gate cannot parse dates nothing, so nothing can be shown to postdate
    // it. As a string it ranked below the verdict and granted coverage.
    let (out, code) = run(
        "unparsable-arrival.json",
        payload("2026-01-02T00:30", "2026-01-02T01:00:00Z"),
    );
    assert_eq!(
        code, 2,
        "an unparsable check-suite timestamp must block, not certify the head.\n{out}"
    );
    assert!(out.contains("BLOCKED"), "{out}");
    assert!(
        !out.contains("HEAD COVERED"),
        "coverage was granted from a timestamp the gate could not read.\n{out}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A bot saying that its review did NOT run is not a finding, and covers no head.
///
/// Codex out of quota posts "You have reached your Codex usage limits for code reviews" as
/// a top-level comment, and CodeRabbit answers a trigger it cannot serve with "Review rate
/// limited." folded under "Action not completed". Both bots are listed in VERDICT_BOTS, so
/// neither comment fell under the notification-bot exemption, and each was a PR-level claim
/// needing a disposition dated after it. On 2026-09-02 #898 carried three Codex quota
/// notices and the gate reported all three UNANSWERED (aws #46 one more); the author
/// cleared them with a NOT-A-DEFECT review answering text that claimed nothing. A gate that
/// blocks on ordinary bot traffic gets switched off, which is why the last-word rule was
/// withdrawn (AGENTS.md).
///
/// A notice is matched as a whole body, line for line, and belongs to the bot that posts
/// it: the same words with a finding appended stay claimable, and a notice still covers no
/// head. CodeRabbit's summary comment carries its notice as a blockquote, so the payload
/// between the notice markers is parsed against the line shapes CodeRabbit posts; a quoted
/// line that is not one of them (`> P1: this drops the last row`) makes the comment
/// claimable again. The shell harness (scripts/test-check-review-threads.sh, "finding 44")
/// carries the full matrix; these four run in CI.
#[test]
fn a_bot_notice_that_no_review_ran_is_neither_a_finding_nor_coverage() {
    require_jq();
    // Bodies as the bots posted them: the two-line Codex notice (#898 at 05:20Z, aws #46),
    // its one-line form (#898 at 05:16Z), and CodeRabbit's rate-limited reply (#847).
    const CODEX_LIMIT: &str = r#""You have reached your Codex usage limits for code reviews. You can see your limits in the [Codex usage dashboard](https://chatgpt.com/codex/cloud/settings/usage).\nTo continue using code reviews, add credits to your account and enable them for code reviews in your [settings](https://chatgpt.com/codex/cloud/settings/code-review).""#;
    const CODEX_LIMIT_SHORT: &str = r#""You have reached your Codex usage limits for code reviews. You can see your limits in the [Codex usage dashboard](https://chatgpt.com/codex/cloud/settings/usage).""#;
    const CR_LIMITED: &str = r#""<!-- This is an auto-generated reply by CodeRabbit -->\n<!-- CodeRabbit review command invocation: 03686aea-15d0-48ab-a4bd-bf524726db31 -->\n<details>\n<summary>⚠️ Action not completed</summary>\n\nReview rate limited.\n\n> Note: CodeRabbit is an incremental review system and does not re-review already reviewed commits. This command is applicable only when automatic reviews are paused.\n\n</details>""#;
    const COVERED: &str = r#"[{"author":{"login":"reviewer"},"state":"APPROVED","submittedAt":"2026-01-02T00:40:00Z","body":"","commit":{"oid":"deadbeef"}}]"#;
    let comment = |login: &str, kind: &str, at: &str, body: &str| {
        format!(
            r#"{{"author":{{"login":"{login}","__typename":"{kind}"}},"createdAt":"{at}","updatedAt":"{at}","body":{body}}}"#
        )
    };
    // The head, the check suite dating its arrival, the reviews on it, and the comments as
    // both reads of them (the gate re-reads the comments after paging and compares).
    let payload = |reviews: &str, comments: &str| {
        format!(
            r#"{{"data":{{"repository":{{"pullRequest":{{"author":{{"login":"me"}},"headRefOid":"deadbeef","commits":{{"nodes":[{{"commit":{{"committedDate":"2026-01-02T00:00:00Z","checkSuites":{{"nodes":[{{"createdAt":"2026-01-02T00:30:00Z"}}]}}}}}}]}},"reviewThreads":{{"nodes":[]}},"reviews":{{"nodes":{reviews}}},"comments":{{"nodes":[{comments}]}},"recheck":{{"comments":{{"nodes":[{comments}]}}}}}}}}}}}}"#
        )
    };

    let dir = std::env::temp_dir().join(format!("fcvm-gate-notice-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let run = |name: &str, body: String| {
        let f = dir.join(name);
        std::fs::write(&f, body).unwrap();
        let out = Command::new("bash")
            .arg(repo_root().join("scripts/check-review-threads.sh"))
            .arg("--from-file")
            .arg(&f)
            .output()
            .expect("check-review-threads.sh must be runnable");
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (combined, out.status.code().unwrap_or(-1))
    };

    // The #898 shape: quota notices around a finding that was answered, on a covered head.
    // The last notice postdates the disposition, so under the old rule nothing answered it.
    let comments = [
        comment(
            "chatgpt-codex-connector",
            "Bot",
            "2026-01-02T01:00:00Z",
            CODEX_LIMIT,
        ),
        comment(
            "reviewer",
            "User",
            "2026-01-02T01:10:00Z",
            r#""P1: this drops the last row""#,
        ),
        comment("coderabbitai", "Bot", "2026-01-02T01:15:00Z", CR_LIMITED),
        comment(
            "me",
            "User",
            "2026-01-02T01:20:00Z",
            r#""RED-VERIFIED: tests/row.rs""#,
        ),
        comment(
            "chatgpt-codex-connector",
            "Bot",
            "2026-01-02T01:30:00Z",
            CODEX_LIMIT_SHORT,
        ),
    ]
    .join(",");
    let (out, code) = run("notices-answered.json", payload(COVERED, &comments));
    assert_eq!(
        code, 0,
        "a bot's notice that its review did not run claims nothing, so a PR whose one \
         finding is answered and whose head is covered is CLEAR. Demanding a disposition \
         for a quota notice is the gate blocking on ordinary bot traffic.\n{out}"
    );
    assert!(out.contains("CLEAR"), "{out}");

    // A notice covers no head: the same quota notice on a PR nobody reviewed leaves the
    // head unreviewed, and the gate must say so rather than read the notice as a result.
    let (out, code) = run(
        "notice-unreviewed.json",
        payload(
            "[]",
            &comment(
                "chatgpt-codex-connector",
                "Bot",
                "2026-01-02T01:00:00Z",
                CODEX_LIMIT,
            ),
        ),
    );
    assert_eq!(
        code, 1,
        "a quota notice is not a review of the head; a PR with no other review result \
         must block as an unreviewed head.\n{out}"
    );
    assert!(out.contains("UNREVIEWED HEAD"), "{out}");
    assert!(
        !out.contains("HEAD COVERED"),
        "a quota notice was read as a no-findings verdict.\n{out}"
    );

    // The match is the whole body: the notice with a finding appended is a finding.
    let with_finding = format!(
        "{}{}",
        CODEX_LIMIT.trim_end_matches('"'),
        r#"\n\nP1: this drops the last row""#
    );
    let (out, code) = run(
        "notice-plus-finding.json",
        payload(
            COVERED,
            &comment(
                "chatgpt-codex-connector",
                "Bot",
                "2026-01-02T01:00:00Z",
                &with_finding,
            ),
        ),
    );
    assert_eq!(
        code, 1,
        "the quota notice followed by a finding is a finding; only the exact notice body \
         is exempt.\n{out}"
    );
    assert!(out.contains("carry no disposition"), "{out}");

    // CodeRabbit's summary comment before any review has run holds the notice as a
    // blockquote (#874). Accepting every line that merely starts with ">" exempted the
    // whole comment from dispositions, so a finding quoted among the notice's own lines
    // was never answered. The payload is parsed instead.
    const CR_SUMMARY_NOTICE: &str = r#""<!-- This is an auto-generated comment: summarize by coderabbit.ai -->\n<!-- This is an auto-generated comment: rate limited by coderabbit.ai -->\n\n> [!WARNING]\n> ## Review limit reached\n> \n> **Next included review available in 53 minutes.**\n> \n> <details>\n> <summary>View limit details</summary>\n> \n> **Limit details:** You\u2019ve used the included review currently available.\n> \n> </details>\n\n<!-- end of auto-generated comment: rate limited by coderabbit.ai -->""#;
    let (out, code) = run(
        "summary-notice.json",
        payload(
            COVERED,
            &comment(
                "coderabbitai",
                "Bot",
                "2026-01-02T01:00:00Z",
                CR_SUMMARY_NOTICE,
            ),
        ),
    );
    assert_eq!(
        code, 0,
        "a summary comment holding nothing but a rate-limit notice says no review ran; \
         it claims nothing and needs no disposition.\n{out}"
    );
    assert!(out.contains("CLEAR"), "{out}");

    let smuggled = CR_SUMMARY_NOTICE.replace(
        r"> </details>\n\n<!-- end of",
        r"> </details>\n> \n> P1: this drops the last row\n\n<!-- end of",
    );
    assert_ne!(smuggled, CR_SUMMARY_NOTICE, "the fixture edit must apply");
    let (out, code) = run(
        "summary-notice-plus-finding.json",
        payload(
            COVERED,
            &comment("coderabbitai", "Bot", "2026-01-02T01:00:00Z", &smuggled),
        ),
    );
    assert_eq!(
        code, 1,
        "a finding quoted inside the notice is still a finding; exempting every line that \
         starts with a quote marker removes it from the disposition check and reports \
         CLEAR.\n{out}"
    );
    assert!(out.contains("carry no disposition"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A region removed before classification is a region the classifier never read.
///
/// Finding 44 parsed the notice PAYLOAD against shapes, because accepting every line that
/// starts with ">" let `> P1: this drops the last row` ride inside a notice and need no
/// disposition. That closed one site. Three more regions were removed from the body before
/// any shape ran: the CodeRabbit tips block (stripped from the summary-notice residue), the
/// folded "About Codex in GitHub" block (stripped by both Codex paths), and HTML comments.
/// A finding placed in either of the first two vanished the same way, and the About-block
/// one was worse than an exemption: the comment stayed a Codex verdict, so it still bound
/// no-findings coverage to the head.
///
/// The invariant is that a body is exempt only when the classifier accounted for every byte
/// of it. Each removal now names a declared region whose content must match that region's
/// line shapes; a region holding anything else makes the whole body claimable. HTML
/// comments are declared unrendered, so nothing inside one is a claim anyone can read.
/// scripts/gate-discard-sites.sh enumerates every discarding call in VERDICT_JQ and blocks
/// on one that is neither the accounting primitive nor a listed normalization, so the next
/// stripping step fails a test unless it declares a region.
///
/// The shell harness (scripts/test-check-review-threads.sh, "finding 45") carries the full
/// matrix, including the review-summary table and the unlisted-tips-line case; these run in
/// CI.
#[test]
fn a_region_stripped_before_classification_is_validated_or_the_body_stays_claimable() {
    require_jq();
    // The tips block as CodeRabbit posts it, under a rate-limit notice (#874, #901).
    const CR_SUMMARY_NOTICE: &str = r#""<!-- This is an auto-generated comment: summarize by coderabbit.ai -->\n<!-- This is an auto-generated comment: rate limited by coderabbit.ai -->\n\n> [!WARNING]\n> ## Review limit reached\n> \n> **Next included review available in 53 minutes.**\n\n<!-- end of auto-generated comment: rate limited by coderabbit.ai -->\n\n<!-- tips_start -->\n\n---\n\nThanks for using [CodeRabbit](https://coderabbit.ai?utm_source=oss&utm_medium=github&utm_campaign=ejc3/fcvm&utm_content=901)! It's free for OSS, and your support helps us grow. If you like it, consider giving us a shout-out.\n\n<sub>Comment `@coderabbitai help` to get the list of available commands.</sub>\n\n<!-- tips_end -->""#;
    // Codex's legacy no-findings verdict with its folded About block (#867 and earlier).
    const CODEX_VERDICT: &str = r#""Codex Review: Didn't find any major issues. Bravo.\n\n**Reviewed commit:** `deadbeef`\n\n<details> <summary>ℹ️ About Codex in GitHub</summary>\n<br/>\n\n[Your team has set up Codex to review pull requests in this repo](https://chatgpt.com/codex/cloud/settings/general). Reviews are triggered when you\n- Open a pull request for review\n- Mark a draft as ready\n- Comment \"@codex review\".\n\nIf Codex has suggestions, it will comment; otherwise it will react with 👍.\n\n</details>""#;
    const COVERED: &str = r#"[{"author":{"login":"reviewer"},"state":"APPROVED","submittedAt":"2026-01-02T00:40:00Z","body":"","commit":{"oid":"deadbeef"}}]"#;
    let comment = |login: &str, body: &str| {
        format!(
            r#"{{"author":{{"login":"{login}","__typename":"Bot"}},"createdAt":"2026-01-02T01:00:00Z","updatedAt":"2026-01-02T01:00:00Z","body":{body}}}"#
        )
    };
    let payload = |reviews: &str, comments: &str| {
        format!(
            r#"{{"data":{{"repository":{{"pullRequest":{{"author":{{"login":"me"}},"headRefOid":"deadbeef","commits":{{"nodes":[{{"commit":{{"committedDate":"2026-01-02T00:00:00Z","checkSuites":{{"nodes":[{{"createdAt":"2026-01-02T00:30:00Z"}}]}}}}}}]}},"reviewThreads":{{"nodes":[]}},"reviews":{{"nodes":{reviews}}},"comments":{{"nodes":[{comments}]}},"recheck":{{"comments":{{"nodes":[{comments}]}}}}}}}}}}}}"#
        )
    };
    let dir = std::env::temp_dir().join(format!("fcvm-gate-region-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let run = |name: &str, body: String| {
        let f = dir.join(name);
        std::fs::write(&f, body).unwrap();
        let out = Command::new("bash")
            .arg(repo_root().join("scripts/check-review-threads.sh"))
            .arg("--from-file")
            .arg(&f)
            .output()
            .expect("check-review-threads.sh must be runnable");
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (combined, out.status.code().unwrap_or(-1))
    };

    // A finding inside the tips block. The residue check ran after the block was removed,
    // so the whole comment passed as a notice and the claim was never answered.
    let tips_finding = CR_SUMMARY_NOTICE.replace(
        r"\n<sub>Comment",
        r"\nP1: this drops the last row\n\n<sub>Comment",
    );
    assert_ne!(
        tips_finding, CR_SUMMARY_NOTICE,
        "the fixture edit must apply"
    );
    let (out, code) = run(
        "tips-finding.json",
        payload(COVERED, &comment("coderabbitai", &tips_finding)),
    );
    assert_eq!(
        code, 1,
        "a finding inside the tips block is a finding: the block is removed before the \
         notice shapes run, so nothing evaluated it.\n{out}"
    );
    assert!(out.contains("carry no disposition"), "{out}");

    // A finding APPENDED to a line that is itself a listed shape. This is what the anchors
    // in each shape list are for: unanchored, the shape matches the head of the line and
    // the rest of it rides along, which is round 1 of this class with a different marker.
    let tips_trailing = CR_SUMMARY_NOTICE.replace(
        r"available commands.</sub>",
        r"available commands.</sub> P1: this drops the last row",
    );
    assert_ne!(
        tips_trailing, CR_SUMMARY_NOTICE,
        "the fixture edit must apply"
    );
    let (out, code) = run(
        "tips-trailing.json",
        payload(COVERED, &comment("coderabbitai", &tips_trailing)),
    );
    assert_eq!(
        code, 1,
        "a shape must match the WHOLE line: a finding appended to a listed tips line is \
         still a finding.\n{out}"
    );
    assert!(out.contains("carry no disposition"), "{out}");

    // A finding inside the About Codex block. This one also bound coverage to the head.
    let about_finding = CODEX_VERDICT.replace(
        r"\nIf Codex has suggestions",
        r"\nP1: this drops the last row\n\nIf Codex has suggestions",
    );
    assert_ne!(about_finding, CODEX_VERDICT, "the fixture edit must apply");
    let (out, code) = run(
        "about-finding.json",
        payload("[]", &comment("chatgpt-codex-connector", &about_finding)),
    );
    assert_eq!(
        code, 1,
        "a finding inside the About Codex block is a finding, and a comment carrying one \
         is not a no-findings verdict of the head.\n{out}"
    );
    assert!(
        !out.contains("HEAD COVERED"),
        "coverage was granted by a comment whose About block was never read.\n{out}"
    );

    // The negative cases the exemption exists for: the same bodies with the regions as the
    // bots post them stay exempt, and the verdict still covers the head.
    let (out, code) = run(
        "tips-clean.json",
        payload(COVERED, &comment("coderabbitai", CR_SUMMARY_NOTICE)),
    );
    assert_eq!(
        code, 0,
        "a rate-limit notice with the tips block CodeRabbit actually posts says no review \
         ran; it claims nothing and needs no disposition.\n{out}"
    );
    assert!(out.contains("CLEAR"), "{out}");
    let (out, code) = run(
        "about-clean.json",
        payload("[]", &comment("chatgpt-codex-connector", CODEX_VERDICT)),
    );
    assert_eq!(
        code, 0,
        "Codex's no-findings verdict with the About block it actually posts still covers \
         the head.\n{out}"
    );
    assert!(out.contains("HEAD COVERED"), "{out}");

    // The structural pin: a stripping step added later must declare a region.
    let out = Command::new("bash")
        .arg(repo_root().join("scripts/gate-discard-sites.sh"))
        .output()
        .expect("gate-discard-sites.sh must be runnable");
    assert!(
        out.status.success(),
        "every call in VERDICT_JQ that discards content must be the accounting primitive \
         or a listed normalization.\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(&dir);
}
