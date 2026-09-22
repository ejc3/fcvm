//! CI coverage invariants for `.github/workflows/ci.yml`.
//!
//! AGENTS.md makes stacked PRs the default: "All work goes in stacked PRs. Each
//! new PR should be based on the previous one, not main." A `pull_request:`
//! trigger's `branches:` filter is matched against the PR's **base** branch, so
//! `branches: [main]` skips every CI job for exactly the PRs the documented
//! workflow tells people to open.
//!
//! This is not hypothetical. On 2026-08-08, PR #752 (base `kernel-7.0.14`)
//! presented a check set with zero failures and was merged on that basis. Its
//! head sha had run `safety-check` and nothing else — `lint`, `packaging`,
//! `host`, `host-root` and `container` had never run at all. It carried three
//! rustfmt violations, which surfaced only once the change reached a PR whose
//! base *was* main. "No failing checks" and "the checks ran" are different
//! claims, and a base-branch filter is what pries them apart.

use serde_norway::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

fn workflow_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(".github/workflows")
        .join(name)
}

fn parse_workflow(name: &str) -> Value {
    let path = workflow_path(name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    serde_norway::from_str(&text)
        .unwrap_or_else(|e| panic!("{} is not valid YAML: {e}", path.display()))
}

/// Return the `on:` mapping.
///
/// YAML 1.1 resolves the bare word `on` to boolean true; YAML 1.2 keeps it a
/// string. Accept either spelling, but **fail** if neither is present rather
/// than returning an empty map — a check that cannot locate what it inspects
/// has no basis for passing.
fn triggers(workflow: &Value) -> &Value {
    workflow
        .get("on")
        .or_else(|| workflow.get(Value::Bool(true)))
        .expect("workflow has no `on:` block — cannot evaluate trigger coverage")
}

fn workflow_job<'a>(workflow: &'a Value, name: &str) -> &'a Value {
    workflow
        .get("jobs")
        .and_then(|jobs| jobs.get(name))
        .unwrap_or_else(|| panic!("workflow has no `{name}` job"))
}

#[test]
fn daily_benchmarks_require_results_from_the_container_target_mount() {
    let daily = parse_workflow("weekly.yml");
    let steps = workflow_job(&daily, "benchmarks")["steps"]
        .as_sequence()
        .expect("benchmark steps");
    let upload = steps
        .iter()
        .find(|step| {
            step["uses"]
                .as_str()
                .is_some_and(|action| action.starts_with("actions/upload-artifact@"))
        })
        .expect("benchmark result upload");
    assert_eq!(
        upload["with"]["path"].as_str(),
        Some("/tmp/fcvm-container-target/criterion/"),
        "upload the target directory mounted by container-bench"
    );
    assert_eq!(
        upload["with"]["if-no-files-found"].as_str(),
        Some("error"),
        "a daily benchmark with no saved results must fail"
    );
}

/// A base-branch filter on `pull_request` silently excludes stacked PRs.
#[test]
fn ci_runs_on_pull_requests_regardless_of_base_branch() {
    let ci = parse_workflow("ci.yml");
    let pull_request = triggers(&ci)
        .get("pull_request")
        .expect("ci.yml has no `pull_request:` trigger — PRs would get no CI at all");

    // A bare `pull_request:` (null) means "every PR", which is what we want.
    if pull_request.is_null() {
        return;
    }

    let mapping = pull_request
        .as_mapping()
        .expect("`pull_request:` is neither null nor a mapping — unexpected shape");

    for key in ["branches", "branches-ignore"] {
        assert!(
            !mapping.contains_key(Value::from(key)),
            "ci.yml restricts `pull_request` with `{key}:`, which is matched against the PR's \
             BASE branch. AGENTS.md makes stacked PRs the default, so this skips lint/host/\
             container for every stacked PR while still reporting a check set with no failures. \
             Remove the filter; use per-job `if:` conditions if some job must be narrowed."
        );
    }
}

/// Every job a merge depends on must exist here AND still gate `Summary`.
///
/// Guards the other half of the same failure: keeping the trigger open but
/// letting a gating job drift out of the gate. Checking only "the job is
/// defined in this file" is too weak — a refactor can leave `fc-mock` defined
/// while dropping it from `summary.needs`, at which point Summary no longer
/// even waits for it.
///
/// Membership in `summary.needs` is necessary but NOT sufficient: `needs` only
/// makes Summary *wait*, and with `if: always()` Summary then reports its own
/// result regardless of theirs. `summary_fails_when_a_gating_job_fails` below
/// covers the second half.
/// Skipping draft PRs is only safe if marking one ready re-triggers CI.
///
/// `pull_request` defaults to opened, synchronize, reopened. `ready_for_review`
/// is NOT in that set, so a workflow that skips drafts without adding it leaves
/// a PR that was drafted and then marked ready sitting with no checks, forever,
/// and GitHub renders that as nothing failing. It is the same green-by-absence
/// hole this file already pins for `branches:` and `paths-ignore:`: a check set
/// that cannot fail because it never ran.
#[test]
fn skipping_drafts_requires_ready_for_review() {
    let ci = parse_workflow("ci.yml");
    let text = std::fs::read_to_string(workflow_path("ci.yml")).expect("ci.yml must be readable");

    // Does anything in this workflow branch on the draft flag?
    let skips_drafts = text.contains("pull_request.draft");
    if !skips_drafts {
        return;
    }

    let pull_request = triggers(&ci)
        .get("pull_request")
        .expect("ci.yml has no `pull_request:` trigger");
    let types = pull_request
        .get("types")
        .and_then(Value::as_sequence)
        .map(|list| {
            list.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    assert!(
        types.iter().any(|t| t == "ready_for_review"),
        "ci.yml skips draft PRs but its `pull_request` types are {types:?}. Without \
         `ready_for_review`, marking a draft ready fires no event, so the PR keeps \
         zero checks and reads as green. Add it, or stop skipping drafts."
    );

    // The default types are implicit only when `types:` is absent. Once it is
    // present, every needed event must be listed or it is silently dropped.
    for needed in ["opened", "synchronize", "reopened"] {
        assert!(
            types.iter().any(|t| t == needed),
            "ci.yml pins `pull_request` types to {types:?}, which drops `{needed}`. \
             Listing types replaces the default set rather than extending it, so a \
             {needed} event would now run nothing."
        );
    }
}

/// Excluding a path from the expensive matrix must ROUTE it, not silence it.
///
/// bench/** no longer sets `code`, because the self-hosted matrix does not
/// exercise the benchmark harness. That is only safe while some job still runs
/// for a bench-only PR. Otherwise such a PR gets Summary and actionlint alone,
/// its four test files never execute, and the result reads as green because
/// nothing that could fail was scheduled. That is the same hole this file pins
/// for `branches:` and `paths-ignore:`.
///
/// Measured when this was written: `grep -c "bench/" ci.yml` was 0 while
/// bench/chromium held 262 passing tests, including the MakefileBenchGraph
/// structural pin AGENTS.md relies on.
#[test]
fn a_path_excluded_from_the_matrix_still_gets_a_check() {
    let ci = parse_workflow("ci.yml");
    let text = std::fs::read_to_string(workflow_path("ci.yml")).expect("ci.yml must be readable");

    // Only binding once bench is classified separately from code.
    if !text.contains("bench=true") {
        return;
    }

    let jobs = ci.get("jobs").expect("ci.yml has no jobs");
    let bench_job = jobs.get("bench-tests").expect(
        "ci.yml classifies bench/** out of `code` but has no `bench-tests` job, so a \
                 bench-only PR would run nothing that can fail",
    );

    let condition = bench_job
        .get("if")
        .and_then(Value::as_str)
        .expect("`bench-tests` has no `if:`; it must run exactly when bench changed");
    assert!(
        condition.contains("changes.outputs.bench"),
        "`bench-tests` does not gate on `changes.outputs.bench` ({condition:?}), so it either \
         never runs or always runs; neither routes a bench-only PR to the check that can fail"
    );

    let summary_needs = jobs
        .get("summary")
        .and_then(|s| s.get("needs"))
        .and_then(Value::as_sequence)
        .map(|l| l.iter().filter_map(Value::as_str).collect::<Vec<_>>())
        .unwrap_or_default();
    assert!(
        summary_needs.contains(&"bench-tests"),
        "`summary` does not depend on `bench-tests` (needs = {summary_needs:?}), so a failing \
         bench suite would not fail the run's required check and would be advisory only"
    );
}

#[test]
fn gating_jobs_live_in_the_pull_request_workflow() {
    let ci = parse_workflow("ci.yml");
    triggers(&ci)
        .get("pull_request")
        .expect("ci.yml has no `pull_request:` trigger");

    let jobs = ci
        .get("jobs")
        .and_then(Value::as_mapping)
        .expect("ci.yml has no `jobs:` mapping");

    let needs: Vec<String> = jobs
        .get(Value::from("summary"))
        .and_then(|s| s.get("needs"))
        .and_then(Value::as_sequence)
        .expect("ci.yml `summary` job has no `needs:` list — nothing aggregates the gates")
        .iter()
        .map(|v| {
            v.as_str()
                .expect("a `summary.needs` entry is not a string")
                .to_string()
        })
        .collect();

    // The floor. Shrinking this set is a deliberate act that must be argued for
    // in review, not something a rename can do silently.
    for job in [
        "lint",
        "packaging",
        "fc-mock",
        "host",
        "host-root",
        "container",
    ] {
        assert!(
            jobs.contains_key(Value::from(job)),
            "ci.yml no longer defines the `{job}` job. If it moved to another workflow, that \
             workflow must also trigger on `pull_request` with no base-branch filter, or \
             stacked PRs lose the check while still reporting no failures."
        );
        assert!(
            needs.iter().any(|n| n == job),
            "`{job}` is defined but is no longer in `summary.needs`, so it no longer gates \
             anything: Summary can go green while it fails or never runs. Add it back, or \
             remove the gate deliberately and update this floor list in the same commit."
        );
    }

    // Anything Summary waits on must actually exist, or the gate is a no-op.
    for n in &needs {
        assert!(
            jobs.contains_key(Value::from(n.as_str())),
            "`summary.needs` lists `{n}`, which is not defined in ci.yml"
        );
    }
}

/// Every self-hosted job that checks out must first repair workspace ownership.
///
/// Self-hosted runners keep their workspace between jobs. fcvm's privileged
/// tests write root-owned files into it (`artifacts/fc-agent` and friends), so
/// the next `actions/checkout` fails: `git clean -ffdx` gets "Permission
/// denied" and the "recreate the repository" fallback gets EACCES. The job dies
/// at checkout, before it builds anything.
///
/// ci.yml has guarded its self-hosted jobs this way for a long time. kernels.yml
/// never got the guard, and **every Build Kernels run from 2026-06-11 onward
/// failed at checkout** — so the FICLONE >4 GiB fix, merged to main on
/// 2026-06-11, was never compiled into a kernel. Two months of "the fix is in
/// main" while every deployed kernel still truncated at `u32::MAX`.
#[test]
fn self_hosted_checkouts_repair_workspace_ownership_first() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".github/workflows");
    let entries =
        std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()));

    let mut checked = 0usize;
    for entry in entries {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("yml") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let wf: Value = match serde_norway::from_str(&std::fs::read_to_string(&path).unwrap()) {
            Ok(v) => v,
            Err(e) => panic!("{name} is not valid YAML: {e}"),
        };
        let Some(jobs) = wf.get("jobs").and_then(Value::as_mapping) else {
            continue;
        };

        for (job_name, job) in jobs {
            // Only self-hosted jobs share a persistent workspace.
            let runs_on = job
                .get("runs-on")
                .map(|v| format!("{v:?}"))
                .unwrap_or_default();
            if !runs_on.contains("self-hosted") {
                continue;
            }
            let Some(steps) = job.get("steps").and_then(Value::as_sequence) else {
                continue;
            };
            // Find the first checkout step; anything before it is pre-checkout.
            let checkout_at = steps.iter().position(|s| {
                s.get("uses")
                    .and_then(Value::as_str)
                    .is_some_and(|u| u.starts_with("actions/checkout"))
            });
            let Some(idx) = checkout_at else { continue };
            checked += 1;

            // Both words must appear in the SAME command. Matching them anywhere
            // in the step's script is not enough: `weekly.yml`'s bench-vm job has
            // `sudo rm -rf ${{ github.workspace }}/...` on one line and
            // `sudo chown -R $USER ~/.cargo/advisory-db*` on another, which
            // satisfies a whole-block substring check while chowning nothing in
            // the workspace. That false negative is precisely what this test
            // exists to prevent, so it must not commit it itself.
            let guarded = steps[..idx].iter().any(|s| {
                s.get("run").and_then(Value::as_str).is_some_and(|r| {
                    r.lines()
                        // Strip comments first: a commented-out repair still
                        // contains both words and chowns nothing. The sibling
                        // gh-probe test already skips comment lines for the
                        // same reason; a guard that a `#` disables is not a
                        // guard.
                        .map(|line| line.split('#').next().unwrap_or(""))
                        .any(|line| line.contains("chown") && line.contains("workspace"))
                })
            });
            let job_label = job_name.as_str().unwrap_or("<job>");
            assert!(
                guarded,
                "{name}: self-hosted job `{job_label}` runs actions/checkout with no preceding \
                 workspace-ownership repair. Root-owned leftovers from a privileged run make \
                 checkout fail with EACCES, and the job dies before building. Add the \
                 `Fix workspace permissions (pre-checkout)` step used by ci.yml."
            );
        }
    }

    assert!(
        checked > 0,
        "found no self-hosted checkout steps to inspect — the walk is broken, and a check that \
         inspects nothing must not report success"
    );
}

/// A `gh` existence probe must not send its error to `/dev/null`.
///
/// `kernels.yml` decided whether to build a kernel with
/// `if gh release view "$TAG" &>/dev/null; then ... else "does not exist"`.
/// That step has no `working-directory`, and every checkout lands in a
/// subdirectory (`path: fcvm`), so `gh` could not infer the repository and
/// failed for a reason unrelated to existence. The redirect discarded the
/// error and the `else` branch reported "does not exist" — for releases that
/// demonstrably did exist. The build then ran to completion and died at the
/// release step:
///
/// ```text
/// Release kernel-nested-6.18.3-aarch64-0fc501348cc2 does not exist   <- step 9
/// a release with the same tag name already exists: ...               <- step 12
/// ```
///
/// That is how Build Kernels failed on 2026-06-14 and 2026-08-07 — the same
/// "cannot tell, so assume the permissive answer" shape as the base-branch
/// filter and the Summary job.
#[test]
fn gh_existence_probes_do_not_discard_their_error() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".github/workflows");
    let mut probes = 0usize;

    for entry in std::fs::read_dir(&dir).expect("read workflows dir") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("yml") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let text = std::fs::read_to_string(&path).expect("read workflow");

        for (i, line) in text.lines().enumerate() {
            let l = line.trim();
            // Inspect commands, not prose. A shell comment explaining the old
            // broken probe is not itself a broken probe — without this the test
            // flags the very comment documenting the fix.
            if l.starts_with('#') {
                continue;
            }
            if !l.contains("gh ") || !l.contains("view") {
                continue;
            }
            probes += 1;
            assert!(
                !(l.contains("&>/dev/null")
                    || l.contains("> /dev/null 2>&1")
                    || l.contains(">/dev/null 2>&1")),
                "{name}:{}: `{l}` discards gh's error, so a failure for any reason other than \
                 non-existence is indistinguishable from \"it does not exist\". Capture the \
                 output, branch on \"not found\", and fail the step on anything else.",
                i + 1
            );
        }
    }

    assert!(
        probes > 0,
        "found no `gh ... view` probes to inspect — the scan is broken, and a check that \
         inspects nothing must not report success"
    );
}

/// `Summary` must actually fail when something it gates on failed.
///
/// `needs:` makes Summary wait; `if: always()` makes it run even when a
/// dependency failed. Together those mean Summary reports *its own* success
/// while a gating job is red — unless it explicitly inspects `needs.*.result`.
///
/// It did not. Across the 40 most recent ci.yml runs, 8 finished with
/// `Summary=success` over genuinely failed jobs:
///
/// ```text
/// run 31271693501: Summary=success but FAILED: Host-Root-arm64-SnapshotEnabled
/// run 31262066685: Summary=success but FAILED: Lint, Container-x64, Container-arm64
/// run 31266285914: Summary=success but FAILED: Container-arm64, Container-x64
/// ```
///
/// Anything treating "Summary green" as "CI green" was reading a gate that
/// could not fail — the same shape as a `CodeRabbit pass` from a review that
/// never started.
#[test]
fn summary_fails_when_a_gating_job_fails() {
    let ci = parse_workflow("ci.yml");
    let summary = ci
        .get("jobs")
        .and_then(|j| j.get("summary"))
        .expect("ci.yml has no `summary` job");

    let steps = summary
        .get("steps")
        .and_then(Value::as_sequence)
        .expect("`summary` job has no steps");

    let conds: Vec<&str> = steps
        .iter()
        .filter_map(|s| s.get("if").and_then(Value::as_str))
        .filter(|c| c.contains("needs.*.result"))
        .collect();

    assert!(
        !conds.is_empty(),
        "ci.yml's `summary` job never inspects `needs.*.result`. With `if: always()` it \
         therefore reports success no matter what its gating jobs did — observed in 8 of the \
         last 40 runs, including one where Lint failed. Add a step conditioned on \
         `contains(needs.*.result, 'failure')` that exits non-zero."
    );

    // Require each non-success terminal state independently. Accepting
    // `failure OR cancelled` would let a later edit drop the `failure` arm while
    // keeping `cancelled`, and this test would still pass while Summary went
    // green over a failed Lint — exactly the regression it exists to prevent.
    for state in ["failure", "cancelled"] {
        assert!(
            conds.iter().any(|c| c.contains(state)),
            "`summary` inspects `needs.*.result` but never checks for `{state}`, so a {state} \
             gating job still yields a green Summary. Each non-success terminal state must be \
             checked on its own, not as one arm of an `||` that a later edit can halve."
        );
    }

    // The `if:` is only half the gate: the step it guards must actually FAIL.
    // Mutating its `exit 1` to `exit 0` left every assertion above satisfied --
    // the condition still matched, the step still ran, and Summary went green
    // over a failed Lint. actionlint cannot object either; `exit 0` is valid
    // shell. So the body is checked too: the failing step's script must end by
    // exiting non-zero, not by succeeding.
    let fail_step_run = steps
        .iter()
        .filter_map(Value::as_mapping)
        .filter(|s| {
            // The GATE step alone checks both terminal states. Two diagnostic
            // steps also fire on `failure` and deliberately end `exit 0` so
            // they cannot preempt the gate -- matching on `failure` alone
            // selects the first of those instead.
            s.get(Value::from("if"))
                .and_then(Value::as_str)
                .is_some_and(|c| {
                    c.contains("needs.*.result") && c.contains("failure") && c.contains("cancelled")
                })
        })
        .filter_map(|s| s.get(Value::from("run")).and_then(Value::as_str))
        .next()
        .expect("the failure-checking step has no `run:` body");
    let last_line = fail_step_run
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or_default()
        .trim();
    assert_eq!(
        last_line, "exit 1",
        "the gate step's script ends with `{last_line}`, not `exit 1`; the condition fires, \
         the error annotation prints, and the job then SUCCEEDS -- Summary green over a \
         failed gating job"
    );
}

/// A diagnostic that cannot run is worse than no diagnostic: it is silent in
/// exactly the case it was written for, and its silence reads as "nothing to
/// report". The runner-loss step shipped that way for one review round. It runs
/// inside `summary`, whose own checkout happens AFTER the gate step that exits
/// 1, so without a checkout of its own the script it invokes does not exist,
/// python exits ENOENT, and the annotation never appears on any run.
#[test]
fn runner_loss_diagnosis_can_actually_run() {
    let ci = parse_workflow("ci.yml");
    let summary = workflow_job(&ci, "summary");
    let steps = summary
        .get("steps")
        .and_then(Value::as_sequence)
        .expect("`summary` job has no steps");

    let diagnose_index = steps
        .iter()
        .position(|s| {
            s.get("name")
                .and_then(Value::as_str)
                .is_some_and(|n| n.contains("Diagnose runner loss"))
        })
        .expect(
            "ci.yml's `summary` job has no `Diagnose runner loss` step, so a dead runner agent \
             stays indistinguishable from a test failure",
        );

    let checkout_before = steps[..diagnose_index].iter().any(|s| {
        s.get("uses")
            .and_then(Value::as_str)
            .is_some_and(|u| u.starts_with("actions/checkout"))
    });
    assert!(
        checkout_before,
        "`Diagnose runner loss` runs before any checkout in the `summary` job, so the classifier \
         it invokes is not on disk. The step then fails to ENOENT and prints nothing, on every \
         run, forever."
    );

    let run = steps[diagnose_index]
        .get("run")
        .and_then(Value::as_str)
        .expect("`Diagnose runner loss` has no `run` block");

    // Every script path the step names must exist in the repo. A path typo
    // (`fcvm/scripts/...` when the checkout lands at the workspace root) is the
    // same unrunnable-check bug wearing different clothes.
    let repo_root = workflow_path("ci.yml")
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .expect("cannot locate repo root from workflow path")
        .to_path_buf();
    let mut checked = 0;
    for token in run.split_whitespace() {
        let candidate = token.trim_matches(|c| c == '"' || c == '\'');
        if !candidate.ends_with(".py") {
            continue;
        }
        checked += 1;
        assert!(
            repo_root.join(candidate).is_file(),
            "`Diagnose runner loss` invokes `{candidate}`, which does not exist relative to the \
             checkout root. The step would exit ENOENT and emit no diagnosis."
        );
    }
    assert!(
        checked > 0,
        "`Diagnose runner loss` names no .py script, so this test verified nothing. If the step \
         changed shape, update the check rather than letting it pass vacuously."
    );
}

/// The infrastructure classifier decides whether a failed CI run is retried or
/// handed to a secret-bearing code fixer. Its fixture suite must execute in the
/// ordinary pull-request gate, not remain a manual-only Make target.
#[test]
fn ci_runs_infrastructure_classifier_fixtures_on_ubuntu() {
    let ci = parse_workflow("ci.yml");
    let lint = workflow_job(&ci, "lint");

    assert_eq!(
        lint.get("runs-on").and_then(Value::as_str),
        Some("ubuntu-latest"),
        "the classifier fixtures must run on a GitHub-hosted Ubuntu runner"
    );

    let steps = lint
        .get("steps")
        .and_then(Value::as_sequence)
        .expect("ci.yml `lint` job has no steps");
    let fixture_step = steps
        .iter()
        .find(|step| {
            step.get("run")
                .and_then(Value::as_str)
                .is_some_and(|run| run.trim() == "make test-ci-infrastructure")
        })
        .expect(
            "ci.yml `lint` never runs `make test-ci-infrastructure`; the privileged CI verdict gate has no fixture coverage in CI",
        );
    assert_eq!(
        fixture_step
            .get("working-directory")
            .and_then(Value::as_str),
        Some("fcvm"),
        "the classifier fixture step must run from the checked-out repository"
    );
    assert_ne!(
        fixture_step
            .get("continue-on-error")
            .and_then(Value::as_bool),
        Some(true),
        "classifier fixture failures must fail the lint gate"
    );
}

/// Infrastructure retries must be bounded, trusted, and mutually exclusive
/// with the secret-bearing Claude fixer.
///
/// `workflow_run` executes with repository secrets even when the failed run
/// came from a pull request. The classifier therefore checks out only the
/// default branch and inspects the failed run through the API. Its output must
/// gate ci-fix so an infrastructure-only failure cannot both rerun and ask
/// Claude to edit code.
#[test]
fn claude_infrastructure_retry_is_trusted_bounded_and_gates_ci_fix() {
    let claude = parse_workflow("claude.yml");
    let classifier = workflow_job(&claude, "classify-ci-failure");

    assert_eq!(
        classifier.get("needs").and_then(Value::as_str),
        Some("safety-check"),
        "the infrastructure classifier must inherit the centralized eligibility gate"
    );
    let condition = classifier
        .get("if")
        .and_then(Value::as_str)
        .expect("classify-ci-failure has no job condition");
    for required in [
        "needs.safety-check.outputs.eligible == 'true'",
        "github.event_name == 'workflow_run'",
        "github.event.workflow_run.head_repository.full_name == github.repository",
        "github.event.workflow_run.conclusion == 'failure'",
        "github.event.workflow_run.name == 'CI'",
    ] {
        assert!(
            condition.contains(required),
            "classify-ci-failure is missing its `{required}` trust/failure gate"
        );
    }

    let permissions = classifier
        .get("permissions")
        .expect("classify-ci-failure has no explicit token permissions");
    assert_eq!(
        permissions.get("actions").and_then(Value::as_str),
        Some("write"),
        "rerunning failed jobs requires an explicitly scoped actions:write token"
    );
    assert_eq!(
        permissions.get("contents").and_then(Value::as_str),
        Some("read"),
        "the trusted classifier checkout needs contents:read and nothing broader"
    );
    let concurrency = classifier
        .get("concurrency")
        .expect("classify-ci-failure has no duplicate-delivery serialization");
    let group = concurrency
        .get("group")
        .and_then(Value::as_str)
        .expect("classify-ci-failure concurrency has no group");
    for identity in [
        "github.event.workflow_run.id",
        "github.event.workflow_run.run_attempt",
    ] {
        assert!(
            group.contains(identity),
            "classifier concurrency does not include `{identity}`"
        );
    }
    assert_eq!(
        concurrency
            .get("cancel-in-progress")
            .and_then(Value::as_bool),
        Some(false),
        "duplicate classifier deliveries must serialize instead of cancelling mid-rerun"
    );

    let steps = classifier
        .get("steps")
        .and_then(Value::as_sequence)
        .expect("classify-ci-failure has no steps");
    let checkout = steps
        .iter()
        .find(|step| {
            step.get("uses")
                .and_then(Value::as_str)
                .is_some_and(|uses| uses.starts_with("actions/checkout@"))
        })
        .expect("classify-ci-failure never checks out its classifier");
    let checkout_with = checkout
        .get("with")
        .expect("classifier checkout has no `with:` configuration");
    assert_eq!(
        checkout_with.get("ref").and_then(Value::as_str),
        Some("${{ github.event.repository.default_branch }}"),
        "a privileged workflow_run job must execute the classifier from the trusted default branch"
    );
    assert_eq!(
        checkout_with
            .get("persist-credentials")
            .and_then(Value::as_bool),
        Some(false),
        "the classifier checkout must not persist its actions:write credential"
    );
    assert!(
        steps.iter().all(|step| {
            step.get("uses").and_then(Value::as_str) != Some("actions/create-github-app-token@v3")
        }),
        "the classifier should use its narrowly scoped GITHUB_TOKEN, not mint an app token"
    );
    for run in steps
        .iter()
        .filter_map(|step| step.get("run").and_then(Value::as_str))
    {
        assert!(
            !run.contains("${{ github.event"),
            "event fields must enter classifier shell through env:, never expression interpolation"
        );
    }

    let classify_step = steps
        .iter()
        .find(|step| step.get("id").and_then(Value::as_str) == Some("classify"))
        .expect("classify-ci-failure has no `classify` output step");
    assert!(
        classify_step
            .get("run")
            .and_then(Value::as_str)
            .is_some_and(|run| run.contains("scripts/classify_ci_failure.py")),
        "the workflow must call the deterministic classifier"
    );

    let rerun_step = steps
        .iter()
        .find(|step| {
            step.get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| name.contains("Rerun failed infrastructure"))
        })
        .expect("classify-ci-failure has no bounded rerun step");
    let rerun_condition = rerun_step
        .get("if")
        .and_then(Value::as_str)
        .expect("the rerun step is unconditional");
    assert!(
        rerun_condition.contains("steps.classify.outputs.rerun == 'true'"),
        "the rerun step must be disabled for genuine and second-attempt failures"
    );
    let rerun = rerun_step
        .get("run")
        .and_then(Value::as_str)
        .expect("the rerun step has no command");
    for required in [
        "CURRENT_ATTEMPT",
        "CURRENT_STATUS",
        "gh run rerun",
        "--failed",
    ] {
        assert!(
            rerun.contains(required),
            "the rerun step is missing its `{required}` one-shot invariant"
        );
    }

    let ci_fix = workflow_job(&claude, "ci-fix");
    let needs: Vec<&str> = ci_fix
        .get("needs")
        .and_then(Value::as_sequence)
        .expect("ci-fix.needs must include both safety and classification gates")
        .iter()
        .map(|need| need.as_str().expect("ci-fix.needs entry is not a string"))
        .collect();
    for required in ["safety-check", "classify-ci-failure"] {
        assert!(
            needs.contains(&required),
            "ci-fix no longer waits for `{required}`"
        );
    }
    let ci_fix_condition = ci_fix
        .get("if")
        .and_then(Value::as_str)
        .expect("ci-fix has no job condition");
    for required in [
        "needs.classify-ci-failure.outputs.classification != 'infrastructure'",
        "github.event.workflow_run.name == 'CI'",
    ] {
        assert!(
            ci_fix_condition.contains(required),
            "ci-fix is missing its `{required}` classification/workflow gate"
        );
    }
}

/// Every PR must produce exactly one `Summary` check run.
///
/// ci.yml used to carry a `paths-ignore` on its `pull_request` trigger, so a PR
/// whose every changed file matched that list produced **no check runs at all**
/// — not even a skipped Summary — and GitHub reported it CLEAN. PR #785 (a
/// two-file actions/setup-node bump touching only `.github/workflows/claude*.yml`)
/// was mechanically mergeable that way. Same claim-confusion as the base-branch
/// filter above: "no failing checks" and "the checks ran" are different claims.
///
/// The fix is per-JOB skipping, not per-workflow: the trigger fires for every
/// PR, a `changes` job decides whether the VM matrix is meaningful, and Summary
/// is always present so it can be a required check. A second workflow that also
/// emits `Summary` is NOT an acceptable substitute: `paths` fires when ANY file
/// matches while `paths-ignore` skips only when EVERY file matches, so a PR
/// touching both `src/` and a doc would emit two `Summary` check runs and make
/// the required-check result ambiguous.
#[test]
fn every_pull_request_produces_exactly_one_summary() {
    let ci = parse_workflow("ci.yml");
    let pull_request = triggers(&ci)
        .get("pull_request")
        .expect("ci.yml has no `pull_request:` trigger — PRs would get no CI at all");

    if let Some(mapping) = pull_request.as_mapping() {
        assert!(
            !mapping.contains_key(Value::from("paths-ignore")),
            "ci.yml restricts `pull_request` with `paths-ignore:`. A PR whose every changed \
             file matches it then produces zero check runs and reads as CLEAN (PR #785). Skip \
             the expensive jobs with the `changes` job instead, so Summary is always present."
        );
        assert!(
            !mapping.contains_key(Value::from("paths")),
            "ci.yml restricts `pull_request` with `paths:`, so PRs outside that list get no \
             Summary at all"
        );
    }

    // No other workflow may render a competing check of the same name.
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".github/workflows");
    for entry in std::fs::read_dir(&dir).expect("read workflows dir") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("yml") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if name == "ci.yml" {
            continue;
        }
        let wf: Value = serde_norway::from_str(&std::fs::read_to_string(&path).unwrap())
            .unwrap_or_else(|e| panic!("{name} is not valid YAML: {e}"));
        let Some(jobs) = wf.get("jobs").and_then(Value::as_mapping) else {
            continue;
        };
        for (_, job) in jobs {
            let renders_summary = job.get("name").and_then(Value::as_str) == Some("Summary");
            assert!(
                !renders_summary,
                "{name} also renders a check named `Summary`. Two workflows emitting the same \
                 check name make a required-check result ambiguous, and their path filters \
                 cannot be kept complementary (`paths` fires on ANY match, `paths-ignore` \
                 skips only on EVERY match), so a mixed PR emits both."
            );
        }
    }
}

/// The VM matrix is skipped per-job, and only for paths it cannot speak to.
///
/// This is the other half of the fix above: dropping `paths-ignore` must not
/// mean running hours of self-hosted VM tests to validate a doc typo. Each
/// expensive job is gated on `changes.outputs.code`, and Summary keeps gating
/// them — a skipped `needs` is already treated as non-failure by the Summary
/// step, while a failed one still fails it.
#[test]
fn expensive_jobs_are_gated_on_the_changes_job() {
    let ci = parse_workflow("ci.yml");
    let jobs = ci
        .get("jobs")
        .and_then(Value::as_mapping)
        .expect("ci.yml has no `jobs:` mapping");

    let changes = jobs
        .get(Value::from("changes"))
        .expect("ci.yml has no `changes` job to decide whether the matrix is meaningful");
    let detect = format!("{changes:?}");
    for ignored in [
        "scripts/claude-assistant/",
        ".github/workflows/claude",
        ".claude/",
        "docs/",
        "*.md",
    ] {
        assert!(
            detect.contains(ignored),
            "the `changes` job no longer recognises `{ignored}` as a path the VM matrix cannot \
             speak to, so PRs touching only it will run the full matrix"
        );
    }

    for job in [
        "lint",
        "packaging",
        "fc-mock",
        "host",
        "host-root",
        "container",
    ] {
        let cond = jobs
            .get(Value::from(job))
            .and_then(|j| j.get("if"))
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("ci.yml job `{job}` has no `if:` gate"));
        assert!(
            cond.contains("needs.changes.outputs.code == 'true'"),
            "ci.yml job `{job}` is not gated on the `changes` job, so a docs-only PR runs it"
        );
        // A substring check tests presence, not effect: prefixing `false && `
        // keeps the asserted text intact while the condition evaluates false on
        // every PR -- the job never runs again and its absence reads as green.
        // Mutation testing walked that straight past this assertion. Nothing
        // legitimate ever puts a constant boolean in a gate, so reject any.
        // Whitespace-normalized once, so `false  &&` and `false &&` read alike.
        let normalized = cond.split_whitespace().collect::<Vec<_>>().join(" ");
        for poison in ["false &&", "&& false", "false ||", "|| true", "true ||"] {
            assert!(
                !normalized.contains(poison),
                "ci.yml job `{job}`'s gate contains the constant `{poison}`: the condition \
                 mentions the changes output but can never (or always) run regardless of it"
            );
        }
    }
}

/// `.github/workflows/claude*.yml` changes must get a check that can fail.
///
/// Those files are exactly the ones the VM matrix says nothing about, so they
/// skip it — which must not decay into "nothing checks them". `actionlint`
/// lives in ci.yml (not in claude-lint.yml) precisely so `Summary` gates it:
/// for a claude-workflow-only PR it is the one gating job that still runs.
#[test]
fn workflow_changes_get_a_relevant_gated_check() {
    let ci = parse_workflow("ci.yml");
    let jobs = ci
        .get("jobs")
        .and_then(Value::as_mapping)
        .expect("ci.yml has no `jobs:` mapping");

    let actionlint = jobs.get(Value::from("actionlint")).expect(
        "ci.yml no longer defines an `actionlint` job, so a workflow-only PR skips the \
                 matrix and nothing else can fail for it",
    );
    assert!(
        actionlint.get("if").is_none(),
        "the `actionlint` job must not be gated on `changes`: workflow-only PRs are exactly \
         the case it exists to cover"
    );

    let needs: Vec<String> = jobs
        .get(Value::from("summary"))
        .and_then(|s| s.get("needs"))
        .and_then(Value::as_sequence)
        .expect("ci.yml `summary` job has no `needs:` list")
        .iter()
        .map(|v| v.as_str().expect("needs entry").to_string())
        .collect();
    assert!(
        needs.iter().any(|n| n == "actionlint"),
        "`actionlint` is defined but is not in `summary.needs`, so Summary can go green while \
         it fails"
    );
}

/// Summary must be able to pass when the VM matrix is skipped.
///
/// Making Summary run on every PR (so it can be a required check) means it now
/// runs for PRs where `changes` skips the matrix. Its artifact steps analyse
/// that matrix's output, and `analyze_ci_vms.py` exits non-zero on an empty
/// directory — so on the first docs-only PR after that change, Summary failed
/// at `Analyze CI run` while every gating job was correctly skipped. A
/// required check that cannot pass for a whole class of PR blocks them all.
#[test]
fn summary_artifact_steps_are_gated_on_the_changes_job() {
    let ci = parse_workflow("ci.yml");
    let steps = workflow_job(&ci, "summary")
        .get("steps")
        .and_then(Value::as_sequence)
        .expect("ci.yml `summary` job has no steps");

    let mut checked = 0usize;
    for step in steps {
        let name = step.get("name").and_then(Value::as_str).unwrap_or("");
        let uses = step.get("uses").and_then(Value::as_str).unwrap_or("");
        let touches_artifacts = uses.contains("download-artifact") || name == "Analyze CI run";
        if !touches_artifacts {
            continue;
        }
        checked += 1;
        let cond = step
            .get("if")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("summary step `{name}` has no `if:` gate"));
        assert!(
            cond.contains("needs.changes.outputs.code == 'true'"),
            "summary step `{name}` consumes the VM matrix's artifacts but is not gated on the \
             `changes` job, so it runs — and fails on an empty artifact directory — for every \
             PR that skips the matrix"
        );
    }

    assert!(
        checked >= 2,
        "expected to find the download-artifact and Analyze CI run steps in `summary`; found \
         {checked}, so this test is not inspecting what it claims"
    );
}

/// Every self-hosted job that runs podman must clear the DEFAULT rootless store
/// before tests — ONCE, and only after its containers config is fully written.
/// The AMI can bake a contaminated store (it is snapshotted from the RUNNING
/// builder since 8a9c564f), and the first `podman build` that resolves to it
/// dies with `chown .../overlay/l: operation not permitted` (#792/#805,
/// 2026-08-12). Placement matters as much as presence: invoked BEFORE the
/// config writes, the script's `podman system reset` tore state down against
/// the wrong layout and every later podman call failed with "database static
/// dir ... does not match" (exit 125 before any test ran, 2026-08-13). The
/// pinned invariant is therefore "immediately after every `podman system
/// migrate`", which is the last line of each job's podman configuration.
#[test]
fn self_hosted_setup_steps_clear_the_default_podman_store() {
    let ci = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/.github/workflows/ci.yml"
    ))
    .expect("read ci.yml");
    let migrates = ci.matches("podman system migrate").count();
    let hygiene = ci.matches("runner-podman-hygiene.sh").count();
    assert_eq!(
        migrates, hygiene,
        "every `podman system migrate` ({migrates}) must be followed by \
         scripts/runner-podman-hygiene.sh (found {hygiene} calls); a job without it inherits \
         the AMI's contaminated default store, and a call anywhere else runs against the \
         wrong storage config"
    );
    assert!(
        hygiene >= 3,
        "expected at least 3 wired jobs, found {hygiene}"
    );
    // Adjacency within the STEP, not just equal counts: global substring
    // counting can pass when one setup block loses its call and a comment
    // elsewhere adds an occurrence. Bound each check at the next step header.
    for (idx, _) in ci.match_indices("podman system migrate") {
        let rest = &ci[idx..];
        let step_end = rest.find("\n      - name:").unwrap_or(rest.len());
        // The hygiene call must be the NEXT executable line (comments allowed):
        // an intervening command could recreate podman state after the heal.
        let next_command = rest[..step_end]
            .split_once('\n')
            .map(|(_, following)| following)
            .unwrap_or_default()
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty() && !line.starts_with('#'));
        assert_eq!(
            next_command,
            Some("./fcvm/scripts/runner-podman-hygiene.sh"),
            "a `podman system migrate` at byte {idx} is not immediately followed by \
             the hygiene invocation"
        );
    }
}

/// The hygiene script must heal a poisoned graphroot state db WITHOUT touching
/// the image layers — the persistent volume's expensive content.
///
/// RED-verified against the previous script version (podman system reset +
/// default-store rm): reset either refused on the very mismatch it was meant
/// to clear ("database static dir \"\" does not match", 2026-08-13, every
/// arm64 job) or, where the db was healthy, deleted the layer cache this test
/// pins as preserved.
#[test]
fn hygiene_script_heals_state_db_and_preserves_layers() {
    let tmp = tempfile::tempdir().expect("create fixture home");
    let home = tmp.path();

    // Fixture: a configured graphroot carrying a state db and an image layer.
    let graphroot = home.join("graphroot");
    std::fs::create_dir_all(graphroot.join("libpod")).unwrap();
    std::fs::write(graphroot.join("libpod/bolt_state.db"), b"poisoned").unwrap();
    std::fs::write(graphroot.join("db.sql"), b"poisoned").unwrap();
    std::fs::create_dir_all(graphroot.join("overlay/abc123")).unwrap();
    std::fs::write(graphroot.join("overlay/abc123/layer"), b"cached layer").unwrap();
    let conf_dir = home.join(".config/containers");
    std::fs::create_dir_all(&conf_dir).unwrap();
    std::fs::write(
        conf_dir.join("storage.conf"),
        format!(
            "[storage]\ndriver = \"overlay\"\ngraphroot = \"{}\"\n",
            graphroot.display()
        ),
    )
    .unwrap();
    // And an AMI-style default store dropping.
    let default_store = home.join(".local/share/containers");
    std::fs::create_dir_all(default_store.join("storage/overlay/l")).unwrap();

    let script = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/scripts/runner-podman-hygiene.sh"
    );
    let out = std::process::Command::new(script)
        .env("HOME", home)
        // Fixture home: the script's passwd-match guard would (correctly)
        // refuse it; the override is the documented test entry.
        .env("FCVM_HYGIENE_HOME_OVERRIDE", "1")
        .output()
        .expect("run hygiene script");
    assert!(
        out.status.success(),
        "hygiene script failed: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        !graphroot.join("libpod").exists(),
        "poisoned libpod db dir must be removed"
    );
    assert!(
        !graphroot.join("db.sql").exists(),
        "poisoned sqlite db must be removed"
    );
    assert!(
        graphroot.join("overlay/abc123/layer").exists(),
        "image layers are the persistent cache and must survive hygiene"
    );
    assert!(
        !default_store.exists(),
        "the default rootless store is a dropping and must be removed"
    );
}

/// The privileged sweep must refuse a HOME that is not this user's passwd
/// home — a stray HOME export would otherwise aim `sudo rm -rf` at an
/// arbitrary directory. Red-verified against the pre-guard script version:
/// it deleted the fixture store instead of refusing.
#[test]
fn hygiene_script_refuses_a_home_that_is_not_the_users() {
    let tmp = tempfile::tempdir().expect("create fixture home");
    let home = tmp.path();
    let default_store = home.join(".local/share/containers");
    std::fs::create_dir_all(&default_store).unwrap();

    let script = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/scripts/runner-podman-hygiene.sh"
    );
    let out = std::process::Command::new(script)
        .env("HOME", home)
        .env_remove("FCVM_HYGIENE_HOME_OVERRIDE")
        .output()
        .expect("run hygiene script");
    assert!(
        !out.status.success(),
        "a HOME that does not match the passwd entry must be refused"
    );
    assert!(
        default_store.exists(),
        "nothing may be deleted when the guard refuses"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("does not match"),
        "stderr must say why: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Run ci.yml's own `case "$f" in … esac` against a path and report how it
/// classified it.
///
/// This lifts the SHIPPED globs out of the workflow rather than restating them,
/// because the defect this guards is an ordering bug between two arms that are
/// individually correct. A restatement would have the same ordering as whatever
/// the test author believed, and would pass.
fn classify_changed_path(path: &str) -> (bool, bool) {
    let ci = parse_workflow("ci.yml");
    let run = workflow_job(&ci, "changes")
        .get("steps")
        .and_then(Value::as_sequence)
        .expect("`changes` job has no `steps:`")
        .iter()
        .filter_map(|s| s.get("run").and_then(Value::as_str))
        .find(|r| r.contains("case \"$f\" in"))
        .expect("`changes` job no longer classifies paths with a `case` — update this test")
        .to_string();

    let start = run.find("case \"$f\" in").expect("case start");
    let end = run[start..].find("esac").expect("unterminated case") + start + "esac".len();
    let case_block = &run[start..end];

    // The arms echo their own diagnostics ("bench path: …"), which land on
    // stdout first, so key the result off a marker rather than off the leading
    // two words. Reading the first two words instead makes `bench path: f`
    // parse as code=`bench`, bench=`path:` — both false — which reports every
    // correctly-classified bench source file as a defect.
    let program = format!(
        "code=false\nbench=false\nf={path:?}\n{case_block}\necho \"CLASSIFIED $code $bench\"\n"
    );
    let out = std::process::Command::new("bash")
        .arg("-c")
        .arg(&program)
        .output()
        .expect("run the extracted classifier");
    assert!(
        out.status.success(),
        "extracted classifier failed for {path}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let verdict = stdout
        .lines()
        .find_map(|l| l.strip_prefix("CLASSIFIED "))
        .unwrap_or_else(|| panic!("classifier printed no verdict for {path}: {stdout}"));
    let mut fields = verdict.split_whitespace();
    let code = fields.next() == Some("true");
    let bench = fields.next() == Some("true");
    (code, bench)
}

/// A markdown file under `bench/` must route to `bench-tests`, not vanish.
///
/// `case` globs match `/`, so `*.md` in the docs arm swallows
/// `bench/chromium/README.md` before the `bench/*` arm is ever considered. That
/// PR then reports `code=false bench=false`: every gating job skips and Summary
/// renders clean, while `bench/chromium/test_reqbench.py` carries lints that
/// read exactly those files —
/// `test_every_binomial_bound_matches_reqanalyze_clopper_pearson` (REVIEW.md),
/// `test_agents_md_jpeg_figures_match_the_record_run` (AGENTS.md),
/// `test_every_path_cited_in_a_doc_table_is_committed` (AGENTS.md, REVIEW.md)
/// and `test_the_readme_healthcheck_verification_actually_fails` (README.md).
///
/// So the one class of change those lints exist to catch is the one class that
/// runs nothing. That is the green-by-absence hole this file documents twice.
#[test]
fn a_bench_markdown_change_routes_to_the_bench_tests() {
    for path in [
        "bench/chromium/README.md",
        "bench/chromium/AGENTS.md",
        "bench/chromium/REVIEW.md",
        "bench/chromium/report/README.md",
    ] {
        let (code, bench) = classify_changed_path(path);
        assert!(
            bench,
            "`{path}` classified as code={code} bench={bench}: it matches the docs arm before \
             `bench/*`, so a PR touching only it gets zero gating jobs while bench lints that \
             read it exist"
        );
    }
}

/// The arms that surround `bench/*` must keep behaving as they did.
///
/// Reordering a `case` is exactly the kind of fix that trades one hole for
/// another, so pin both directions: bench code still routes to bench, ordinary
/// docs still skip everything, and source still runs the matrix.
#[test]
fn path_classification_holds_on_both_sides_of_the_bench_arm() {
    for (path, want_code, want_bench) in [
        ("bench/chromium/reqbench.py", false, true),
        ("bench/chromium/test_reqbench.py", false, true),
        // README.md and PERFORMANCE.md are LINTED by
        // tests/test_documented_make_targets.rs, so they must reach the matrix.
        ("README.md", true, false),
        ("PERFORMANCE.md", true, false),
        // The Makefile is both code and the subject of MakefileBenchGraph.
        ("Makefile", true, true),
        ("AGENTS.md", false, false),
        ("docs/design.md", false, false),
        (".claude/skills/pr-workflow/SKILL.md", false, false),
        ("scripts/claude-assistant/index.ts", false, false),
        ("src/main.rs", true, false),
        ("scripts/scan-test-log.sh", true, false),
        (".github/workflows/ci.yml", true, false),
    ] {
        let (code, bench) = classify_changed_path(path);
        assert_eq!(
            (code, bench),
            (want_code, want_bench),
            "`{path}` classified as code={code} bench={bench}, expected code={want_code} \
             bench={want_bench}"
        );
    }
}

/// Run the `changes` step's own script with a synthetic environment.
///
/// `gh` is never reached on the fail-open paths (they exit before the API
/// call), which is exactly what makes those paths testable here.
fn run_changes_step(event: &str) -> String {
    let ci = parse_workflow("ci.yml");
    let run = workflow_job(&ci, "changes")
        .get("steps")
        .and_then(Value::as_sequence)
        .expect("`changes` job has no `steps:`")
        .iter()
        .filter_map(|s| s.get("run").and_then(Value::as_str))
        .find(|r| r.contains("case \"$f\" in"))
        .expect("`changes` job no longer classifies paths — update this test")
        .to_string();

    let dir = std::env::temp_dir().join(format!("fcvm-changes-{}-{event}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let out_file = dir.join("github_output");
    std::fs::write(&out_file, "").expect("seed GITHUB_OUTPUT");

    let status = std::process::Command::new("bash")
        .arg("-c")
        .arg(&run)
        .env("EVENT", event)
        .env("GITHUB_OUTPUT", &out_file)
        .env("REPO", "o/r")
        .env("PR", "1")
        .output()
        .expect("run the changes step");
    assert!(
        status.status.success(),
        "changes step failed for event {event}: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let outputs = std::fs::read_to_string(&out_file).expect("read GITHUB_OUTPUT");
    let _ = std::fs::remove_dir_all(&dir);
    outputs
}

/// A fail-open must open BOTH gates, not just the code one.
///
/// Both early exits write `code=true` and never write `bench` at all, so
/// `bench-tests` — gated on `needs.changes.outputs.bench == 'true'` — is
/// SKIPPED on push-to-main, `workflow_dispatch`, the `Build Kernels`
/// `workflow_run`, and on the "empty file list" path whose own comment says it
/// fails toward running the matrix. Summary treats skipped as non-failure, so
/// the bench lints this file routes PRs to would never run on main at all,
/// which matters precisely because the documented stacked-PR routine
/// force-merges with `--admin` after cancelling the PR-side run.
///
/// "Fail open" has to mean every gate, or it is just a differently-shaped hole.
#[test]
fn a_fail_open_opens_the_bench_gate_too() {
    for event in ["push", "workflow_dispatch", "workflow_run", "schedule"] {
        let outputs = run_changes_step(event);
        assert!(
            outputs.contains("code=true"),
            "event {event}: expected code=true, got {outputs:?}"
        );
        assert!(
            outputs.contains("bench=true"),
            "event {event}: fail-open set code but left bench unset, so bench-tests \
             is skipped and its lints never run: {outputs:?}"
        );
    }
}

/// A rename must be classified by where it came FROM as well as where it went.
///
/// `gh api ... --jq '.[].filename'` reports only the post-rename path, so
/// `git mv src/foo.rs bench/chromium/foo.rs` yields a single `bench/` entry:
/// `code` stays false, the whole VM matrix skips, and a tree that no longer
/// compiles is mergeable behind a green Summary. `previous_filename` is present
/// on exactly the renamed entries and is what closes it.
#[test]
fn renames_are_classified_by_their_source_path_too() {
    let ci = parse_workflow("ci.yml");
    let run = workflow_job(&ci, "changes")
        .get("steps")
        .and_then(Value::as_sequence)
        .expect("`changes` job has no `steps:`")
        .iter()
        .filter_map(|s| s.get("run").and_then(Value::as_str))
        .find(|r| r.contains("gh api"))
        .expect("`changes` job no longer lists changed files");
    // Assert on the --jq ARGUMENT, not on the whole `run:` block. The block now
    // explains previous_filename in prose, so `run.contains("previous_filename")`
    // stayed true even with the query reverted to `.[].filename` -- a test that
    // could not fail for the defect it names.
    let jq = run
        .split("--jq")
        .nth(1)
        .and_then(|rest| {
            let rest = rest.trim_start();
            let quote = rest.chars().next()?;
            rest[1..].split(quote).next()
        })
        .expect("the changed-file step no longer passes a --jq expression");
    assert!(
        jq.contains("previous_filename"),
        "the changed-file --jq expression is {jq:?}, which reads only the post-rename \
         path, so moving a source file into bench/ or docs/ hides it from the \
         classifier and skips the whole matrix"
    );
}

/// The `changes` job must actually EMIT every output the matrix gates on.
///
/// `expensive_jobs_are_gated_on_the_changes_job` checks the CONSUMER side: that
/// each expensive job's `if:` reads `needs.changes.outputs.code`. Nothing checked
/// the PRODUCER side, and the two are wired together by string equality across
/// three places:
///
///   the detect script:  echo "code=$code" >> "$GITHUB_OUTPUT"
///   the job's outputs:  code: ${{ steps.detect.outputs.code }}
///   every consumer:     needs.changes.outputs.code
///
/// A one-character typo in the middle line -- `steps.detect.outputs.cod` -- makes
/// `needs.changes.outputs.code` evaluate to the empty string on every PR. lint,
/// packaging, fc-mock, host, host-root and container all skip, and `summary`
/// goes GREEN having run only actionlint. A complete CI bypass, from one
/// character, that the whole suite passes through: the classification tests lift
/// the `case` block out and append their own marker, so they never execute the
/// output writes at all, and `actionlint -shellcheck=` exits 0.
///
/// Found by mutation testing, not by reading. Both variants -- the typo and
/// deleting the `echo` -- left 21/21 tests passing.
#[test]
fn the_changes_job_emits_every_output_the_matrix_gates_on() {
    let ci = parse_workflow("ci.yml");
    let jobs = ci
        .get("jobs")
        .and_then(Value::as_mapping)
        .expect("ci.yml has no `jobs:` mapping");
    let changes = jobs
        .get(Value::from("changes"))
        .and_then(Value::as_mapping)
        .expect("ci.yml has no `changes` job");

    // Every step's `run:` in the changes job, concatenated: this is where the
    // GITHUB_OUTPUT writes live.
    let scripts: String = changes
        .get(Value::from("steps"))
        .and_then(Value::as_sequence)
        .expect("the `changes` job has no steps")
        .iter()
        .filter_map(Value::as_mapping)
        .filter_map(|s| s.get(Value::from("run")).and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");

    let declared = changes
        .get(Value::from("outputs"))
        .and_then(Value::as_mapping)
        .expect(
            "the `changes` job declares no `outputs:`, so nothing it computes reaches the matrix",
        );

    for (name, expr) in declared {
        let name = name.as_str().expect("output name is not a string");
        let expr = expr.as_str().unwrap_or_default();
        // The declared expression must reference a step output of the SAME name.
        // A typo here is invisible at runtime: GitHub yields "" rather than an error.
        assert!(
            expr.contains(&format!("outputs.{name}")),
            "`changes` declares output `{name}` as `{expr}`, which does not read \
             `outputs.{name}`. Every consumer of `needs.changes.outputs.{name}` will see the \
             empty string, so its gated jobs skip and `summary` goes green having run nothing."
        );
        // And the script must write that key from the COMPUTED variable, not
        // merely somewhere. There are three writes of `code=` in this file: two
        // fail-open branches emit the literal `code=true`, and the main path
        // emits `code=$code`. An earlier version of this assertion accepted any
        // `echo "code="`, so deleting the MAIN write still passed -- the
        // fail-opens covered for it. That is the same disease this test exists
        // to cure, one level up, and mutation testing is what exposed it.
        assert!(
            scripts.contains(&format!("echo \"{name}=${name}\"")),
            "`changes` declares output `{name}` but no step writes `{name}=${name}` to \
             $GITHUB_OUTPUT, so the classifier's verdict never reaches the matrix and every \
             job gated on it skips while `summary` goes green"
        );
        // The fail-open branches must ALSO open this gate. A fail-open that
        // opens only some gates is the hole this file already documents.
        assert!(
            scripts.contains(&format!("echo \"{name}=true\"")),
            "no fail-open branch writes `{name}=true`, so a path the classifier cannot \
             evaluate leaves `{name}` closed and its jobs never run"
        );
    }

    // And the reverse: nothing may gate on an output `changes` does not declare.
    let whole = std::fs::read_to_string(workflow_path("ci.yml")).expect("ci.yml is unreadable");
    let mut consumed: Vec<String> = Vec::new();
    let mut rest = whole.as_str();
    while let Some(at) = rest.find("needs.changes.outputs.") {
        rest = &rest[at + "needs.changes.outputs.".len()..];
        let key: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if !key.is_empty() && !consumed.contains(&key) {
            consumed.push(key);
        }
    }
    assert!(
        !consumed.is_empty(),
        "no job reads `needs.changes.outputs.*` any more, so the classifier gates nothing"
    );
    for key in &consumed {
        assert!(
            declared.contains_key(Value::from(key.as_str())),
            "a job gates on `needs.changes.outputs.{key}`, which the `changes` job does not \
             declare. That expression is the empty string, so the job never runs and its \
             absence reads as success."
        );
    }
}

/// Every apt-get call in a self-hosted job goes through `scripts/ci-apt-get.sh`.
///
/// A self-hosted runner is a freshly booted instance, and its boot-time apt can still hold a
/// lock when the job starts. On 2026-09-13 Host-arm64 on #921 was assigned 74 s after its
/// runner launched and failed in "Install dependencies" with
/// `E: Could not get lock /var/lib/dpkg/lock-frontend. It is held by process 3180 (apt)`.
/// A bare `sudo apt-get` fails on a held lock; the script waits for it.
#[test]
fn self_hosted_apt_calls_wait_for_apt_locks() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".github/workflows");
    let entries =
        std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()));

    let mut checked = 0usize;
    for entry in entries {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("yml") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let wf: Value = serde_norway::from_str(&std::fs::read_to_string(&path).unwrap())
            .unwrap_or_else(|e| panic!("{name} is not valid YAML: {e}"));
        let Some(jobs) = wf.get("jobs").and_then(Value::as_mapping) else {
            continue;
        };
        for (job_name, job) in jobs {
            let runs_on = job
                .get("runs-on")
                .map(|v| format!("{v:?}"))
                .unwrap_or_default();
            if !runs_on.contains("self-hosted") {
                continue;
            }
            let job_label = job_name.as_str().unwrap_or("<job>");
            let Some(steps) = job.get("steps").and_then(Value::as_sequence) else {
                continue;
            };
            for step in steps {
                let Some(run) = step.get("run").and_then(Value::as_str) else {
                    continue;
                };
                for line in run.lines().map(|l| l.split('#').next().unwrap_or("")) {
                    if !line.contains("apt-get") {
                        continue;
                    }
                    checked += 1;
                    assert!(
                        !line
                            .replace("./fcvm/scripts/ci-apt-get.sh", "")
                            .contains("apt-get"),
                        "{name}: self-hosted job `{job_label}` calls apt-get directly: `{}`. \
                         A boot-time apt on a fresh runner can hold the dpkg or lists lock, and \
                         apt-get fails on it at once. Use ./fcvm/scripts/ci-apt-get.sh.",
                        line.trim()
                    );
                    // The path is relative to the workspace root, where `path: fcvm` checks
                    // the repo out.
                    assert!(
                        step.get("working-directory").is_none(),
                        "{name}: job `{job_label}` calls ./fcvm/scripts/ci-apt-get.sh from a step \
                         with a working-directory, where that path does not resolve"
                    );
                }
            }
        }
    }
    assert!(
        checked > 0,
        "found no apt-get calls in self-hosted jobs to inspect. The walk is broken, and a check \
         that inspects nothing must not report success"
    );
}

/// `scripts/ci-apt-get.sh` retries a held apt lock until its deadline, and nothing else.
///
/// Driven with a fake apt-get that records its arguments and plays one outcome per call:
/// `lock` prints apt's held-lock error, `other` prints a different error (both exit 100),
/// and anything else exits 0.
#[test]
fn ci_apt_get_retries_a_held_lock_and_nothing_else() {
    use std::os::unix::fs::PermissionsExt;

    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scripts/ci-apt-get.sh");
    let dir = std::env::temp_dir().join(format!("fcvm-ci-apt-get-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let fake = dir.join("apt-get");
    std::fs::write(
        &fake,
        r#"#!/bin/bash
d=$(dirname "$0")
echo "$*" >> "$d/calls"
step=$(sed -n "$(wc -l < "$d/calls")p" "$d/plan")
case "$step" in
  lock) echo 'E: Could not get lock /var/lib/apt/lists/lock. It is held by process 3180 (apt)'; exit 100 ;;
  other) echo 'E: Unable to locate package no-such-package'; exit 100 ;;
  *) exit 0 ;;
esac
"#,
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

    let run = |plan: &[&str], wait_s: &str| -> (i32, Vec<String>) {
        let _ = std::fs::remove_file(dir.join("calls"));
        std::fs::write(dir.join("plan"), plan.join("\n") + "\n").unwrap();
        let out = std::process::Command::new("bash")
            .arg(&script)
            .arg("update")
            .env("APT_GET", &fake)
            .env("SUDO", "")
            .env("APT_LOCK_RETRY_S", "0")
            .env("APT_LOCK_WAIT", wait_s)
            .output()
            .expect("bash must be runnable");
        let calls = std::fs::read_to_string(dir.join("calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect();
        (out.status.code().unwrap_or(-1), calls)
    };

    let (code, calls) = run(&["lock", "lock", "ok"], "60");
    assert_eq!(
        code, 0,
        "a lock released after two attempts must end in success: {calls:?}"
    );
    assert_eq!(calls.len(), 3, "{calls:?}");
    assert!(
        calls
            .iter()
            .all(|c| c.starts_with("-o DPkg::Lock::Timeout=") && c.ends_with(" update")),
        "every attempt must pass the dpkg lock timeout and the caller's arguments: {calls:?}"
    );

    let (code, calls) = run(&["other", "ok"], "60");
    assert_eq!(
        code, 100,
        "a failure that is not a held lock is apt-get's answer: {calls:?}"
    );
    assert_eq!(calls.len(), 1, "it must not be retried: {calls:?}");

    let held = vec!["lock"; 20];
    let (code, calls) = run(&held, "0");
    assert_eq!(
        code, 100,
        "a lock still held at the deadline must fail with apt-get's status: {calls:?}"
    );
    assert_eq!(calls.len(), 1, "{calls:?}");

    // A retry sleep that crosses the deadline ends the wait. Checking the deadline only before
    // the sleep started one more apt-get after it had passed (CodeRabbit on #925).
    let _ = std::fs::remove_file(dir.join("calls"));
    std::fs::write(dir.join("plan"), held.join("\n") + "\n").unwrap();
    let out = std::process::Command::new("bash")
        .arg(&script)
        .arg("update")
        .env("APT_GET", &fake)
        .env("SUDO", "")
        .env("APT_LOCK_RETRY_S", "2")
        .env("APT_LOCK_WAIT", "1")
        .output()
        .expect("bash must be runnable");
    let calls: Vec<String> = std::fs::read_to_string(dir.join("calls"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(
        out.status.code(),
        Some(100),
        "the last apt-get status is returned: {calls:?}"
    );
    assert_eq!(
        calls.len(),
        1,
        "no apt-get attempt may start after the sleep crossed the deadline: {calls:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// "Save disk I/O record" must save a finished record of this job's own sampler. It ran
/// `pkill -x iostat` and copied `/tmp/fcvm-iostat.log` straight away: the copy raced the
/// sampler's last write, and on a runner that streams jobs, a job whose sampler never
/// started copied the log an earlier job left in `/tmp` (CodeRabbit on #924).
#[test]
fn disk_io_record_is_this_jobs_and_complete() {
    let ci = parse_workflow("ci.yml");
    let dir = std::env::temp_dir().join(format!("fcvm-iostat-record-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).expect("temp dir");
    for (name, body) in [
        // One sample at once, and on SIGTERM a last one half a second later, the way a
        // report in progress lands. It exits by itself after 20 s, so a missed stop leaks
        // nothing.
        (
            "iostat",
            "#!/bin/bash\ntrap 'sleep 0.5; echo last-sample; exit 0' TERM\necho first-sample\nfor _ in $(seq 200); do sleep 0.1; done\n",
        ),
        ("lsblk", "#!/bin/bash\necho nvme0n1\n"),
    ] {
        let path = bin.join(name);
        std::fs::write(&path, body).expect("write fake command");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("make fake command executable");
    }
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let mut problems = Vec::new();
    for job in ["host", "host-root", "container"] {
        let steps = workflow_job(&ci, job)
            .get("steps")
            .and_then(Value::as_sequence)
            .unwrap_or_else(|| panic!("`{job}` job has no `steps:`"));
        let run_of = |name: &str| {
            steps
                .iter()
                .find(|step| step.get("name").and_then(Value::as_str) == Some(name))
                .and_then(|step| step.get("run"))
                .and_then(Value::as_str)
                .unwrap_or_else(|| panic!("`{job}` has no `{name}` step with a `run:` script"))
                .to_string()
        };
        let create = run_of("Create test log directory");
        let save = run_of("Save disk I/O record");
        for (name, script) in [
            ("Create test log directory", &create),
            ("Save disk I/O record", &save),
        ] {
            if script.contains("pkill") {
                problems.push(format!(
                    "`{job}` \"{name}\" signals every iostat on the host, not its own sampler's PID"
                ));
            }
            if script.contains("/tmp/fcvm-iostat") {
                problems.push(format!(
                    "`{job}` \"{name}\" keeps the sampler's files in /tmp, where an earlier job's survive, not $RUNNER_TEMP"
                ));
            }
        }

        let temp = dir.join(job);
        let logs = dir.join(format!("{job}-logs"));
        std::fs::create_dir_all(&temp).expect("runner temp dir");
        let run = |script: &str| {
            std::process::Command::new("bash")
                .arg("-c")
                .arg(script.replace("/tmp/fcvm-test-logs", &logs.display().to_string()))
                .env("PATH", &path)
                .env("RUNNER_TEMP", &temp)
                .output()
                .expect("bash must be runnable")
        };
        let start = create
            .find("setsid")
            .map(|at| &create[at..])
            .unwrap_or_else(|| panic!("`{job}` starts no iostat sampler"));
        let started = run(start);
        assert!(
            started.status.success(),
            "`{job}` sampler start failed: {}",
            String::from_utf8_lossy(&started.stderr)
        );
        std::thread::sleep(std::time::Duration::from_secs(1));
        let saved = run(&save);
        assert!(
            saved.status.success(),
            "`{job}` \"Save disk I/O record\" failed: {}",
            String::from_utf8_lossy(&saved.stderr)
        );
        let record = std::fs::read_to_string(logs.join("iostat.log")).unwrap_or_default();
        if !(record.contains("first-sample") && record.contains("last-sample")) {
            problems.push(format!(
                "`{job}` saved {record:?}: copied before the sampler had finished writing"
            ));
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// Every self-hosted job ci.yml can run, under the names GitHub gives them.
const SELF_HOSTED_JOBS: [&str; 8] = [
    "Host-arm64",
    "Host-x64",
    "Host-Root-arm64-SnapshotDisabled",
    "Host-Root-arm64-SnapshotEnabled",
    "Host-Root-x64-SnapshotDisabled",
    "Host-Root-x64-SnapshotEnabled",
    "Container-arm64",
    "Container-x64",
];

fn job_set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| name.to_string()).collect()
}

/// Run skip-check's planning step with fake `gh` and `git` on PATH and return what it
/// wrote to `$GITHUB_OUTPUT`. The fake `gh` reports one successful PR run, whose commit
/// has this push's tree when `pr_tree_matches`, with `pr_jobs` as that run's jobs. Any
/// other `gh` or `git` call fails the step.
fn run_matrix_plan(
    event: &str,
    pr_tree_matches: bool,
    pr_jobs: &[&str],
) -> BTreeMap<String, String> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let ci = parse_workflow("ci.yml");
    let run = workflow_job(&ci, "skip-check")
        .get("steps")
        .and_then(Value::as_sequence)
        .expect("`skip-check` job has no `steps:`")
        .iter()
        .find(|step| step.get("id").and_then(Value::as_str) == Some("check"))
        .and_then(|step| step.get("run"))
        .and_then(Value::as_str)
        .expect("`skip-check` has no `check` step with a `run:` script")
        .to_string();

    let dir = std::env::temp_dir().join(format!(
        "fcvm-matrix-plan-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    for (name, body) in [
        (
            "gh",
            r#"#!/bin/bash
case "$1 $2" in
  'run list') echo '101 feedface' ;;
  'api repos/o/r/git/commits/feedface') echo "$FAKE_PR_TREE" ;;
  'api repos/o/r/actions/runs/101/jobs') printf '%s\n' "$FAKE_PR_JOBS" ;;
  *) echo "unexpected: gh $*" >&2; exit 1 ;;
esac
"#,
        ),
        (
            "git",
            r#"#!/bin/bash
if [ "$*" = 'rev-parse HEAD^{tree}' ]; then echo push-tree; exit 0; fi
echo "unexpected: git $*" >&2
exit 1
"#,
        ),
    ] {
        let path = dir.join(name);
        std::fs::write(&path, body).expect("write fake command");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("make fake command executable");
    }
    let out_file = dir.join("github_output");
    std::fs::write(&out_file, "").expect("seed GITHUB_OUTPUT");
    let path = format!(
        "{}:{}",
        dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let result = std::process::Command::new("bash")
        .arg("-c")
        .arg(&run)
        .env("PATH", path)
        .env("EVENT", event)
        .env("REPO", "o/r")
        .env("GH_TOKEN", "unused")
        .env("GITHUB_OUTPUT", &out_file)
        .env(
            "FAKE_PR_TREE",
            if pr_tree_matches {
                "push-tree"
            } else {
                "another-tree"
            },
        )
        .env("FAKE_PR_JOBS", pr_jobs.join("\n"))
        .output()
        .expect("run the skip-check planner");
    assert!(
        result.status.success(),
        "the planner failed for {event}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let outputs = std::fs::read_to_string(&out_file).expect("read GITHUB_OUTPUT");
    let _ = std::fs::remove_dir_all(&dir);
    outputs
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

/// The job names a plan expands to. Also checks that every `*_run` flag and `skip`
/// agree with the lists, because GitHub fails a run on an empty matrix rather than
/// skipping the job.
fn planned_jobs(outputs: &BTreeMap<String, String>) -> BTreeSet<String> {
    let mut jobs = BTreeSet::new();
    for (key, prefix) in [
        ("host", "Host"),
        ("host_root", "Host-Root"),
        ("container", "Container"),
    ] {
        let raw = outputs
            .get(key)
            .unwrap_or_else(|| panic!("the planner wrote no `{key}` output: {outputs:?}"));
        let matrix: serde_json::Value = serde_json::from_str(raw)
            .unwrap_or_else(|e| panic!("`{key}` is not a JSON matrix ({e}): {raw}"));
        let names: Vec<String> = if key == "host_root" {
            matrix["include"]
                .as_array()
                .unwrap_or_else(|| panic!("`host_root` has no include list: {raw}"))
                .iter()
                .map(|entry| {
                    let arch = entry["arch"].as_str().expect("entry without arch");
                    let mode = entry["mode"].as_str().expect("entry without mode");
                    let (no_snapshot, runs) = if mode == "SnapshotDisabled" {
                        ("1", "1")
                    } else {
                        ("", "2")
                    };
                    assert_eq!(
                        (
                            entry["fcvm_no_snapshot"].as_str(),
                            entry["test_runs"].as_str()
                        ),
                        (Some(no_snapshot), Some(runs)),
                        "{mode} must set FCVM_NO_SNAPSHOT='{no_snapshot}' and run the suite \
                         {runs} time(s): {entry}"
                    );
                    format!("{prefix}-{arch}-{mode}")
                })
                .collect()
        } else {
            matrix["arch"]
                .as_array()
                .unwrap_or_else(|| panic!("`{key}` has no arch list: {raw}"))
                .iter()
                .map(|arch| format!("{prefix}-{}", arch.as_str().expect("arch is not a string")))
                .collect()
        };
        let flag = if names.is_empty() { "false" } else { "true" };
        assert_eq!(
            outputs.get(&format!("{key}_run")).map(String::as_str),
            Some(flag),
            "`{key}_run` disagrees with `{key}` = {raw}"
        );
        jobs.extend(names);
    }
    let skip = if jobs.is_empty() { "true" } else { "false" };
    assert_eq!(
        outputs.get("skip").map(String::as_str),
        Some(skip),
        "`skip` disagrees with the planned jobs {jobs:?}"
    );
    jobs
}

/// A pull request runs every arm64 job but only ONE x64 job.
///
/// Four x64 metal instances per PR push were most of the x86 CI bill, and three of those
/// jobs run paths the arm64 jobs already cover. Host-Root-x64-SnapshotEnabled is the one
/// kept: it takes the privileged suite through a snapshot miss and a restore, which is
/// where the x86-specific KVM and vCPU state code lives.
#[test]
fn a_pull_request_runs_every_arm64_job_and_one_x64_job() {
    let jobs = planned_jobs(&run_matrix_plan("pull_request", false, &[]));
    let x64: BTreeSet<String> = jobs.iter().filter(|j| j.contains("x64")).cloned().collect();
    assert_eq!(
        x64,
        job_set(&["Host-Root-x64-SnapshotEnabled"]),
        "a pull request must run exactly one x64 job"
    );
    let arm64: BTreeSet<String> = jobs
        .iter()
        .filter(|j| j.contains("arm64"))
        .cloned()
        .collect();
    let every_arm64: BTreeSet<String> = job_set(&SELF_HOSTED_JOBS)
        .into_iter()
        .filter(|j| j.contains("arm64"))
        .collect();
    assert_eq!(
        arm64, every_arm64,
        "a pull request must still run every arm64 job"
    );
}

/// Main runs whatever its PR run of the same tree did not.
///
/// Together the two runs are the full matrix, so x86 is still fully tested before a
/// release, and nothing that already passed on the PR runs again on main. Other jobs in
/// the PR run's list (Lint here) must not change the plan.
#[test]
fn main_runs_the_x64_jobs_its_pull_request_left_out() {
    let pr = planned_jobs(&run_matrix_plan("pull_request", false, &[]));
    let mut reported: Vec<&str> = pr.iter().map(String::as_str).collect();
    reported.push("Lint");
    let main = planned_jobs(&run_matrix_plan("push", true, &reported));
    assert!(
        pr.is_disjoint(&main),
        "main re-ran jobs that already passed on the PR: {:?}",
        pr.intersection(&main).collect::<Vec<_>>()
    );
    let both: BTreeSet<String> = pr.union(&main).cloned().collect();
    assert_eq!(
        both,
        job_set(&SELF_HOSTED_JOBS),
        "the PR run and the main run together must cover the full matrix"
    );
}

/// Nothing is assumed without a passing PR run of the same tree: a push whose tree never
/// passed on a PR, a manual dispatch and the Build Kernels trigger all run everything.
#[test]
fn runs_without_a_passing_pull_request_tree_get_the_full_matrix() {
    assert_eq!(
        planned_jobs(&run_matrix_plan("push", false, &SELF_HOSTED_JOBS)),
        job_set(&SELF_HOSTED_JOBS),
        "a push whose tree never passed on a PR must run the full matrix"
    );
    for event in ["workflow_dispatch", "workflow_run"] {
        assert_eq!(
            planned_jobs(&run_matrix_plan(event, false, &[])),
            job_set(&SELF_HOSTED_JOBS),
            "{event} must run the full matrix"
        );
    }
}

/// When the matching PR run already covered everything, main runs nothing: either that
/// run had the full matrix, or its `changes` job skipped the matrix, which the jobs API
/// reports under the unexpanded matrix names.
#[test]
fn main_skips_the_matrix_when_its_pull_request_needed_nothing_more() {
    assert!(
        planned_jobs(&run_matrix_plan("push", true, &SELF_HOSTED_JOBS)).is_empty(),
        "every self-hosted job already passed for this tree, so main must run none"
    );
    let skipped = [
        "Host-${{ matrix.arch }}",
        "Host-Root-${{ matrix.arch }}-${{ matrix.mode }}",
        "Container-${{ matrix.arch }}",
        "Lint",
    ];
    assert!(
        planned_jobs(&run_matrix_plan("push", true, &skipped)).is_empty(),
        "the PR run skipped the matrix for this tree, so main must skip it too"
    );
}

/// The plan only takes effect if the jobs read it: each self-hosted job must take its
/// matrix from skip-check and check that list's flag before the matrix expands.
#[test]
fn self_hosted_jobs_take_their_matrix_from_the_plan() {
    let ci = parse_workflow("ci.yml");
    let declared = workflow_job(&ci, "skip-check")
        .get("outputs")
        .and_then(Value::as_mapping)
        .expect("`skip-check` declares no outputs");
    for (job, output) in [
        ("host", "host"),
        ("host-root", "host_root"),
        ("container", "container"),
    ] {
        let definition = workflow_job(&ci, job);
        let matrix = definition
            .get("strategy")
            .and_then(|strategy| strategy.get("matrix"))
            .and_then(Value::as_str);
        let wanted = format!("${{{{ fromJSON(needs.skip-check.outputs.{output}) }}}}");
        assert_eq!(
            matrix,
            Some(wanted.as_str()),
            "`{job}` does not take its matrix from skip-check's `{output}` plan"
        );
        let cond = definition
            .get("if")
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert!(
            cond.contains(&format!("needs.skip-check.outputs.{output}_run == 'true'")),
            "`{job}` does not check `{output}_run`, so an empty plan fails the run instead of \
             skipping the job"
        );
        for name in [output.to_string(), format!("{output}_run")] {
            let expr = declared
                .get(Value::from(name.as_str()))
                .and_then(Value::as_str)
                .unwrap_or_default();
            assert_eq!(
                expr,
                format!("${{{{ steps.check.outputs.{name} }}}}"),
                "`skip-check` does not pass the planner's `{name}` output through"
            );
        }
    }
}

/// The SnapshotEnabled lane runs `make clean-test-data` after the suite and before
/// "Upload test logs", so bench-vm starts on a clean slate. While that target also
/// deleted /tmp/fcvm-test-logs, every artifact from the lane lost its per-VM debug
/// logs, including the console of a guest that hung while shutting down.
#[test]
fn cleaning_test_data_keeps_the_test_logs() {
    let makefile = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Makefile"))
        .expect("read Makefile");
    let lines: Vec<&str> = makefile.lines().collect();
    let rule = lines
        .iter()
        .rposition(|line| line.starts_with("clean-test-data:"))
        .expect("Makefile has a clean-test-data rule");
    let recipe: Vec<&str> = lines[rule + 1..]
        .iter()
        .take_while(|line| line.starts_with('\t'))
        .copied()
        .collect();
    assert!(
        recipe.iter().any(|line| line.contains("snapshots prune")),
        "did not find the clean-test-data recipe: {recipe:?}"
    );
    let deletes_logs: Vec<&str> = recipe
        .iter()
        .filter(|line| {
            line.contains("rm ")
                && (line.contains("fcvm-test-logs") || line.contains("TEST_LOG_DIR"))
        })
        .copied()
        .collect();
    assert!(
        deletes_logs.is_empty(),
        "clean-test-data deletes the test logs, which CI uploads after running it: {deletes_logs:?}"
    );
}
