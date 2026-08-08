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

/// The jobs a merge depends on must be reachable from the `pull_request` trigger.
///
/// Guards the other half of the same failure: keeping the trigger open but
/// moving the gating jobs into a workflow that stacked PRs never fire.
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

    for job in [
        "lint",
        "host",
        "host-root",
        "container",
        "packaging",
        "fc-mock",
    ] {
        assert!(
            jobs.contains_key(Value::from(job)),
            "ci.yml no longer defines the `{job}` job. If it moved to another workflow, that \
             workflow must also trigger on `pull_request` with no base-branch filter, or \
             stacked PRs lose the check while still reporting no failures."
        );
    }
}
