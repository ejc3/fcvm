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
use std::path::PathBuf;

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
}
