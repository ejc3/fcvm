#!/usr/bin/env python3
"""
Analyze CI test run artifacts - counts VMs spawned during tests.

Usage:
    python3 scripts/analyze_ci_vms.py /tmp/ci-artifacts
"""
import sys
from pathlib import Path


def main():
    if len(sys.argv) < 2:
        print("Usage: analyze_ci_vms.py <artifacts-dir>")
        sys.exit(1)

    artifacts_dir = Path(sys.argv[1])
    if not artifacts_dir.is_dir():
        print(f"Error: {artifacts_dir} is not a directory")
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
