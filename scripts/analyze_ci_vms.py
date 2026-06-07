#!/usr/bin/env python3
"""
Analyze CI test run artifacts - counts VMs spawned during tests.

Usage:
    python3 scripts/analyze_ci_vms.py [--allow-missing] /tmp/ci-artifacts
"""
import sys
from pathlib import Path


def main():
    flags = [a for a in sys.argv[1:] if a.startswith("--")]
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    allow_missing = "--allow-missing" in flags
    # Reject unknown flags rather than silently discarding them — a typo like
    # `--allow-misssing` must not be quietly ignored back into strict mode.
    unknown = [f for f in flags if f != "--allow-missing"]
    if unknown or len(args) != 1:
        print(
            f"Usage: analyze_ci_vms.py [--allow-missing] <artifacts-dir>"
            + (f"\nUnknown argument(s): {' '.join(unknown)}" if unknown else ""),
            file=sys.stderr,
        )
        sys.exit(1)

    artifacts_dir = Path(args[0])
    if not artifacts_dir.is_dir():
        # Missing artifacts directory. This is only benign when the caller KNOWS
        # the matrix was intentionally skipped (pass --allow-missing). Otherwise
        # the matrix ran but produced no artifacts — e.g. jobs died before
        # creating their log dir, with uploads set to `if-no-files-found: ignore`
        # — which is a real loss of diagnostics, NOT success. Fail loudly so the
        # Summary signal is not silently green (#639).
        if allow_missing:
            print("No CI artifacts to analyze (test matrix skipped)")
            return
        print(
            f"ERROR: artifacts directory {artifacts_dir} is missing, but the test "
            "matrix was expected to run. Test jobs likely failed before uploading "
            "logs. Pass --allow-missing only on the intentional skip path.",
            file=sys.stderr,
        )
        sys.exit(1)

    # Count VMs from log files
    base_vms = 0
    clone_vms = 0
    by_job = {}

    for job_dir in artifacts_dir.iterdir():
        if not job_dir.is_dir() or not job_dir.name.startswith("test-logs-"):
            continue

        job_name = job_dir.name.replace("test-logs-", "")
        job_base = 0
        job_clone = 0

        for log_file in job_dir.glob("*.log"):
            name = log_file.name
            if '-base-' in name:
                job_base += 1
            elif '-clone-' in name:
                job_clone += 1

        if job_base > 0 or job_clone > 0:
            by_job[job_name] = (job_base, job_clone)
            base_vms += job_base
            clone_vms += job_clone

    # Print summary
    print()
    print("=" * 50)
    print("           FCVM CI SUMMARY")
    print("=" * 50)
    print()
    print(f"  VMs spawned: {base_vms} base + {clone_vms} clones = {base_vms + clone_vms} total")
    print()

    if by_job:
        print("  By job:")
        for job, (b, c) in sorted(by_job.items()):
            print(f"    {job}: {b} base + {c} clones")
        print()


if __name__ == '__main__':
    main()
