#!/usr/bin/env python3
"""
Analyze CI test run artifacts.

Usage:
    python3 scripts/analyze_ci_vms.py /tmp/ci-artifacts
"""
import re
import sys
from pathlib import Path


def strip_ansi(text: str) -> str:
    """Remove ANSI escape codes."""
    return re.sub(r'\x1b\[[0-9;]*m', '', text)


def main():
    if len(sys.argv) < 2:
        print("Usage: analyze_ci_vms.py <artifacts-dir>")
        sys.exit(1)

    artifacts_dir = Path(sys.argv[1])
    if not artifacts_dir.is_dir():
        print(f"Error: {artifacts_dir} is not a directory")
        sys.exit(1)

    # Count VMs from log files
    base_vms = []
    clone_vms = []

    for job_dir in artifacts_dir.iterdir():
        if not job_dir.is_dir() or not job_dir.name.startswith("test-logs-"):
            continue
        for log_file in job_dir.glob("*.log"):
            name = log_file.name
            if '-base-' in name:
                base_vms.append(name)
            elif '-clone-' in name:
                clone_vms.append(name)

    # Parse full log for test results
    log_path = artifacts_dir / "full.log"
    tests_passed = 0
    tests_total = 0
    duration = 0

    if log_path.exists():
        content = strip_ansi(log_path.read_text())
        for line in content.split('\n'):
            if 'tests run' in line:
                m = re.search(r'(\d+)\s*tests run:\s*(\d+)\s*passed', line)
                if m:
                    tests_total += int(m.group(1))
                    tests_passed += int(m.group(2))
                d = re.search(r'\[\s*([\d.]+)s\]', line)
                if d:
                    duration += float(d.group(1))

    # Print summary
    print()
    print("=" * 50)
    print("           FCVM CI SUMMARY")
    print("=" * 50)
    print()
    print(f"  Tests:  {tests_passed}/{tests_total} passed")
    print(f"  VMs:    {len(base_vms)} base + {len(clone_vms)} clones")
    print(f"  Time:   {duration:.0f}s")
    print()


if __name__ == '__main__':
    main()
