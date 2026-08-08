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
/// while dropping it from `summary.needs`, at which point it no longer gates
/// anything and this test would still have passed. So assert membership in
/// `summary.needs`, which is what actually makes a job a gate.
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

            let guarded = steps[..idx].iter().any(|s| {
                s.get("run")
                    .and_then(Value::as_str)
                    .is_some_and(|r| r.contains("chown") && r.contains("workspace"))
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
