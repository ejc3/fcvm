#!/usr/bin/env python3
"""
Analyze CI test run.

Usage:
    # Analyze latest run
    python3 scripts/analyze_ci_vms.py

    # Analyze specific run
    python3 scripts/analyze_ci_vms.py 20699186509
"""
import json
import re
import subprocess
import sys
import tempfile
from collections import defaultdict
from pathlib import Path


def run_cmd(cmd: list[str], check: bool = True) -> str:
    """Run command and return stdout."""
    result = subprocess.run(cmd, capture_output=True, text=True, check=check)
    return result.stdout


def strip_ansi(text: str) -> str:
    """Remove ANSI escape codes from text."""
    return re.sub(r'\x1b\[[0-9;]*m', '', text)


def get_run_id(arg: str | None) -> str:
    """Get run ID from argument or fetch latest."""
    if arg:
        return arg
    # Get latest run
    result = run_cmd(["gh", "run", "list", "--limit", "1", "--json", "databaseId"])
    runs = json.loads(result)
    if not runs:
        print("No runs found")
        sys.exit(1)
    return str(runs[0]["databaseId"])


def download_artifacts(run_id: str, dest: Path) -> None:
    """Download test-logs artifacts for a run."""
    # List artifacts for this run
    result = run_cmd(["gh", "api", f"repos/{{owner}}/{{repo}}/actions/runs/{run_id}/artifacts"])
    data = json.loads(result)

    # Download only test-logs artifacts
    for artifact in data.get("artifacts", []):
        name = artifact["name"]
        if name.startswith("test-logs-"):
            artifact_dir = dest / name
            artifact_dir.mkdir(parents=True, exist_ok=True)
            run_cmd(["gh", "run", "download", run_id, "--name", name, "--dir", str(artifact_dir)])


def get_full_log(run_id: str, dest: Path) -> Path:
    """Download full log for a run."""
    log_path = dest / "full.log"
    result = run_cmd(["gh", "run", "view", run_id, "--log"], check=False)
    log_path.write_text(result)
    return log_path


def analyze_logs(artifacts_dir: Path) -> dict:
    """Parse log files and count VMs by type."""
    results = {
        'base_vms': [],
        'clone_vms': [],
        'uffd_servers': [],
        'by_job': defaultdict(lambda: {'base': 0, 'clone': 0, 'uffd': 0}),
        'by_test': defaultdict(lambda: {'base': 0, 'clone': 0}),
    }

    for job_dir in artifacts_dir.iterdir():
        if not job_dir.is_dir() or not job_dir.name.startswith("test-logs-"):
            continue
        job_name = job_dir.name

        for log_file in job_dir.glob("*.log"):
            name = log_file.name

            if '-base-' in name:
                results['base_vms'].append(name)
                results['by_job'][job_name]['base'] += 1
                test = name.split('-base-')[0]
                results['by_test'][test]['base'] += 1

            elif '-clone-' in name:
                results['clone_vms'].append(name)
                results['by_job'][job_name]['clone'] += 1
                if '-clone-' in name:
                    parts = name.split('-clone-')
                    test = parts[0].rsplit('-', 2)[0] if parts[0].count('-') > 2 else parts[0]
                    results['by_test'][test]['clone'] += 1

            elif 'uffd-server' in name:
                results['uffd_servers'].append(name)
                results['by_job'][job_name]['uffd'] += 1

    return results


def analyze_full_log(log_path: Path) -> dict | None:
    """Parse the full CI log for test results and timing."""
    if not log_path.exists():
        return None

    content = strip_ansi(log_path.read_text())

    results = {
        'jobs': {},
        'tests': {'passed': 0, 'failed': 0, 'skipped': 0, 'flaky': 0},
        'pjdfstest': [],
        'nested_kvm': False,
    }

    # Parse job summaries
    for line in content.split('\n'):
        if 'Summary' in line and 'tests run' in line:
            parts = line.split('\t')
            if len(parts) >= 1:
                job = parts[0].strip()
            else:
                continue

            dur_match = re.search(r'\[\s*([\d.]+)s\]', line)
            if not dur_match:
                continue
            duration = float(dur_match.group(1))

            tests_match = re.search(r'(\d+)\s*tests run:\s*(\d+)\s*passed', line)
            if not tests_match:
                continue
            total = int(tests_match.group(1))
            passed = int(tests_match.group(2))

            if job not in results['jobs'] or total > results['jobs'][job]['total']:
                results['jobs'][job] = {'duration': duration, 'total': total, 'passed': passed}

    # Flaky tests
    flaky_matches = re.findall(r'(\d+)\s*flaky', content)
    results['tests']['flaky'] = sum(int(m) for m in flaky_matches)

    # pjdfstest categories
    pjdf_cats = set(re.findall(r'pjdfs-vm-(\w+)', content))
    results['pjdfstest'] = sorted(pjdf_cats)

    # Nested KVM
    if 'test_kvm_available_in_vm' in content and 'PASS' in content:
        results['nested_kvm'] = True

    # Performance metrics (multiple patterns for different log formats)
    results['snapshot_times'] = [float(m.group(1)) for m in re.finditer(r'Snapshot created \(took ([\d.]+)s\)', content)]
    results['clone_health_times'] = [float(m.group(1)) for m in re.finditer(r'health=([\d.]+)s', content)]
    results['boot_times'] = [float(m.group(1)) for m in re.finditer(r'took ([\d.]+)s\)?$', content, re.MULTILINE)]

    # pjdfstest individual test counts (from prove output: "Files=X, Tests=Y")
    pjdf_tests = [int(m.group(1)) for m in re.finditer(r'Tests=(\d+),', content)]
    results['pjdfstest_count'] = sum(pjdf_tests)

    return results


def print_report(run_id: str, vm_results: dict, log_results: dict | None) -> None:
    """Print formatted report."""
    total_base = len(vm_results['base_vms'])
    total_clone = len(vm_results['clone_vms'])
    total_uffd = len(vm_results['uffd_servers'])
    total_vms = total_base + total_clone

    print()
    print("=" * 60)
    print(f"           FCVM CI RUN {run_id}")
    print("=" * 60)
    print()

    # Big numbers summary
    if log_results and log_results.get('jobs'):
        total_tests = sum(j['total'] for j in log_results['jobs'].values())
        total_passed = sum(j['passed'] for j in log_results['jobs'].values())
        total_duration = sum(j['duration'] for j in log_results['jobs'].values())
        pjdf_count = log_results.get('pjdfstest_count', 0)

        print(f"  TESTS:     {total_passed}/{total_tests} passed", end="")
        if pjdf_count:
            print(f" (+ {pjdf_count} pjdfstest)")
        else:
            print()
        print(f"  VMs:       {total_vms} spawned ({total_base} base + {total_clone} clones)")
        print(f"  UFFD:      {total_uffd} memory servers")
        print(f"  DURATION:  {total_duration:.0f}s total test time")
        if log_results['tests']['flaky'] > 0:
            print(f"  FLAKY:     {log_results['tests']['flaky']} tests")
        print()

    # Jobs breakdown
    if log_results and log_results.get('jobs'):
        print("JOBS:")
        for job, data in sorted(log_results['jobs'].items()):
            status = "✓" if data['passed'] == data['total'] else "✗"
            print(f"  {status} {job}: {data['passed']}/{data['total']} in {data['duration']:.0f}s")
        print()

    # VMs by job
    print("VMs BY JOB:")
    for job, counts in sorted(vm_results['by_job'].items()):
        job_short = job.replace('test-logs-', '')
        total = counts['base'] + counts['clone']
        if total > 0:
            print(f"  {job_short}: {counts['base']} base + {counts['clone']} clones")
    print()

    # Performance
    if log_results:
        print("PERFORMANCE:")
        if log_results.get('boot_times'):
            avg = sum(log_results['boot_times']) / len(log_results['boot_times'])
            print(f"  Boot to healthy:    {avg:.1f}s avg (n={len(log_results['boot_times'])})")
        if log_results.get('snapshot_times'):
            avg = sum(log_results['snapshot_times']) / len(log_results['snapshot_times'])
            print(f"  Snapshot creation:  {avg:.1f}s avg (n={len(log_results['snapshot_times'])})")
        if log_results.get('clone_health_times'):
            avg = sum(log_results['clone_health_times']) / len(log_results['clone_health_times'])
            print(f"  Clone to healthy:   {avg:.2f}s avg (n={len(log_results['clone_health_times'])})")
        print()

    # Clone scaling (top 5)
    clone_tests = [(t, c) for t, c in vm_results['by_test'].items() if c['clone'] > 0]
    if clone_tests:
        print("CLONE SCALING (top 5):")
        for test, counts in sorted(clone_tests, key=lambda x: x[1]['clone'], reverse=True)[:5]:
            ratio = counts['clone'] / counts['base'] if counts['base'] > 0 else counts['clone']
            print(f"  {test}: {counts['base']} → {counts['clone']} ({ratio:.0f}x)")
        print()

    # Features
    features = []
    if log_results:
        if log_results.get('pjdfstest'):
            pjdf_count = log_results.get('pjdfstest_count', 0)
            features.append(f"pjdfstest: {pjdf_count} tests in {len(log_results['pjdfstest'])} categories")
        if log_results.get('nested_kvm'):
            features.append("nested KVM: ✓")
    if features:
        print("FEATURES: " + ", ".join(features))
        print()


def main():
    run_id = get_run_id(sys.argv[1] if len(sys.argv) > 1 else None)

    print(f"Analyzing run {run_id}...")

    with tempfile.TemporaryDirectory() as tmpdir:
        dest = Path(tmpdir)

        # Download artifacts
        print("Downloading artifacts...")
        download_artifacts(run_id, dest)

        # Get full log
        print("Downloading full log...")
        log_path = get_full_log(run_id, dest)

        # Analyze
        vm_results = analyze_logs(dest)
        log_results = analyze_full_log(log_path)

        # Report
        print_report(run_id, vm_results, log_results)


if __name__ == '__main__':
    main()
