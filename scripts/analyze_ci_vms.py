#!/usr/bin/env python3
"""
Analyze CI test run from log artifacts.

Usage:
    # Download artifacts first:
    gh run download <run_id> --dir /tmp/ci-artifacts
    
    # Also download full log:
    gh run view <run_id> --log > /tmp/ci-artifacts/full.log
    
    # Then analyze:
    python3 scripts/analyze_ci_vms.py /tmp/ci-artifacts
"""
import re
import sys
from collections import defaultdict
from pathlib import Path


def strip_ansi(text: str) -> str:
    """Remove ANSI escape codes from text."""
    return re.sub(r'\x1b\[[0-9;]*m', '', text)


def analyze_logs(artifacts_dir: str):
    """Parse log files and count VMs by type."""
    results = {
        'base_vms': [],
        'clone_vms': [],
        'uffd_servers': [],
        'by_job': defaultdict(lambda: {'base': 0, 'clone': 0, 'uffd': 0}),
        'by_test': defaultdict(lambda: {'base': 0, 'clone': 0}),
    }
    
    for job_dir in Path(artifacts_dir).iterdir():
        if not job_dir.is_dir():
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


def analyze_full_log(log_path: Path):
    """Parse the full CI log for test results and timing."""
    if not log_path.exists():
        return None
    
    content = strip_ansi(log_path.read_text())
    
    results = {
        'jobs': {},
        'tests': {'passed': 0, 'failed': 0, 'skipped': 0, 'flaky': 0},
        'timings': [],
        'pjdfstest': [],
        'nested_kvm': False,
    }
    
    # Parse job summaries - handle both container format and host format
    # Container: Summary [ 239.109s] 136 tests run: 136 passed, 0 skipped
    # Host-Root: Summary [ 475.100s] 241 tests run: 241 passed (3 flaky), 13 skipped
    for line in content.split('\n'):
        if 'Summary' in line and 'tests run' in line:
            # Extract job name from line start
            parts = line.split('\t')
            if len(parts) >= 1:
                job = parts[0].strip()
            else:
                continue
            
            # Extract duration
            dur_match = re.search(r'\[\s*([\d.]+)s\]', line)
            if not dur_match:
                continue
            duration = float(dur_match.group(1))
            
            # Extract test counts
            tests_match = re.search(r'(\d+)\s*tests run:\s*(\d+)\s*passed', line)
            if not tests_match:
                continue
            total = int(tests_match.group(1))
            passed = int(tests_match.group(2))
            
            # Only keep the highest test count per job (final summary)
            if job not in results['jobs'] or total > results['jobs'][job]['total']:
                results['jobs'][job] = {'duration': duration, 'total': total, 'passed': passed}
    
    # Check for flaky tests
    flaky_matches = re.findall(r'(\d+)\s*flaky', content)
    results['tests']['flaky'] = sum(int(m) for m in flaky_matches)
    
    # pjdfstest categories
    pjdf_cats = set(re.findall(r'pjdfs-vm-(\w+)', content))
    results['pjdfstest'] = sorted(pjdf_cats)
    
    # Nested KVM test
    if 'test_kvm_available_in_vm' in content and 'PASS' in content:
        results['nested_kvm'] = True
    
    # Snapshot timings
    results['snapshot_times'] = []
    for match in re.finditer(r'Snapshot created \(took ([\d.]+)s\)', content):
        results['snapshot_times'].append(float(match.group(1)))
    
    # Clone health times
    results['clone_health_times'] = []
    for match in re.finditer(r'health=([\d.]+)s', content):
        results['clone_health_times'].append(float(match.group(1)))
    
    # VM boot times
    results['boot_times'] = []
    for match in re.finditer(r'VM healthy.*took ([\d.]+)s', content):
        results['boot_times'].append(float(match.group(1)))
    
    return results


def print_report(vm_results, log_results=None):
    """Print formatted report of VM counts and test results."""
    total_base = len(vm_results['base_vms'])
    total_clone = len(vm_results['clone_vms'])
    total_uffd = len(vm_results['uffd_servers'])
    total_vms = total_base + total_clone
    
    print("=" * 70)
    print("                    FCVM CI FULL ANALYSIS REPORT")
    print("=" * 70)
    print()
    
    # Test Results Summary
    if log_results and log_results.get('jobs'):
        print("## TEST RESULTS SUMMARY")
        print(f"┌{'─'*20}┬{'─'*12}┬{'─'*10}┬{'─'*12}┐")
        print(f"│ {'Job':<18} │ {'Duration':>10} │ {'Tests':>8} │ {'Passed':>10} │")
        print(f"├{'─'*20}┼{'─'*12}┼{'─'*10}┼{'─'*12}┤")
        total_tests = 0
        total_passed = 0
        total_duration = 0
        for job, data in sorted(log_results['jobs'].items()):
            dur_str = f"{data['duration']:.1f}s"
            print(f"│ {job:<18} │ {dur_str:>10} │ {data['total']:>8} │ {data['passed']:>10} │")
            total_tests += data['total']
            total_passed += data['passed']
            total_duration += data['duration']
        print(f"├{'─'*20}┼{'─'*12}┼{'─'*10}┼{'─'*12}┤")
        print(f"│ {'TOTAL':<18} │ {total_duration:.1f}s{' '*4} │ {total_tests:>8} │ {total_passed:>10} │")
        print(f"└{'─'*20}┴{'─'*12}┴{'─'*10}┴{'─'*12}┘")
        
        if log_results['tests']['flaky'] > 0:
            print(f"  ⚠️  Flaky tests: {log_results['tests']['flaky']}")
        print()
    
    # VM Count
    print("## VM SPAWN COUNT")
    print(f"┌{'─'*30}┬{'─'*10}┐")
    print(f"│ {'Category':<28} │ {'Count':>8} │")
    print(f"├{'─'*30}┼{'─'*10}┤")
    print(f"│ {'Base VMs':<28} │ {total_base:>8} │")
    print(f"│ {'Clone VMs':<28} │ {total_clone:>8} │")
    print(f"│ {'UFFD Servers':<28} │ {total_uffd:>8} │")
    print(f"├{'─'*30}┼{'─'*10}┤")
    print(f"│ {'TOTAL VMs SPAWNED':<28} │ {total_vms:>8} │")
    print(f"└{'─'*30}┴{'─'*10}┘")
    print()
    
    # By Job
    print("## VMs BY CI JOB")
    print(f"┌{'─'*25}┬{'─'*8}┬{'─'*8}┬{'─'*8}┐")
    print(f"│ {'Job':<23} │ {'Base':>6} │ {'Clone':>6} │ {'UFFD':>6} │")
    print(f"├{'─'*25}┼{'─'*8}┼{'─'*8}┼{'─'*8}┤")
    for job, counts in sorted(vm_results['by_job'].items()):
        job_short = job.replace('test-logs-', '')
        print(f"│ {job_short:<23} │ {counts['base']:>6} │ {counts['clone']:>6} │ {counts['uffd']:>6} │")
    print(f"└{'─'*25}┴{'─'*8}┴{'─'*8}┴{'─'*8}┘")
    print()
    
    # Clone Scaling
    print("## CLONE SCALING TESTS")
    clone_tests = [(t, c) for t, c in vm_results['by_test'].items() if c['clone'] > 0]
    print(f"┌{'─'*30}┬{'─'*8}┬{'─'*8}┬{'─'*10}┐")
    print(f"│ {'Test':<28} │ {'Base':>6} │ {'Clone':>6} │ {'Ratio':>8} │")
    print(f"├{'─'*30}┼{'─'*8}┼{'─'*8}┼{'─'*10}┤")
    for test, counts in sorted(clone_tests, key=lambda x: x[1]['clone'], reverse=True):
        ratio = counts['clone'] / counts['base'] if counts['base'] > 0 else counts['clone']
        ratio_str = f"{ratio:.0f}x"
        print(f"│ {test[:28]:<28} │ {counts['base']:>6} │ {counts['clone']:>6} │ {ratio_str:>8} │")
    print(f"└{'─'*30}┴{'─'*8}┴{'─'*8}┴{'─'*10}┘")
    print()
    
    # Performance
    if log_results:
        print("## PERFORMANCE METRICS")
        if log_results.get('boot_times'):
            avg_boot = sum(log_results['boot_times']) / len(log_results['boot_times'])
            print(f"  VM boot to healthy: avg {avg_boot:.1f}s (n={len(log_results['boot_times'])})")
        if log_results.get('snapshot_times'):
            avg_snap = sum(log_results['snapshot_times']) / len(log_results['snapshot_times'])
            print(f"  Snapshot creation:  avg {avg_snap:.1f}s (n={len(log_results['snapshot_times'])})")
        if log_results.get('clone_health_times'):
            avg_health = sum(log_results['clone_health_times']) / len(log_results['clone_health_times'])
            print(f"  Clone to healthy:   avg {avg_health:.2f}s (n={len(log_results['clone_health_times'])})")
        print()
    
    # pjdfstest
    if log_results and log_results.get('pjdfstest'):
        print("## POSIX COMPLIANCE (pjdfstest)")
        print(f"  Categories tested: {len(log_results['pjdfstest'])}")
        for cat in log_results['pjdfstest']:
            print(f"    - {cat}")
        print()
    
    # Nested KVM
    if log_results and log_results.get('nested_kvm'):
        print("## NESTED VIRTUALIZATION")
        print("  ✅ test_kvm_available_in_vm PASSED")
        print("     /dev/kvm works inside VMs (kvm-arm.mode=nested)")
        print()
    
    # All tests by category
    print("## ALL TESTS BY CATEGORY")
    by_base = sorted(vm_results['by_test'].items(), key=lambda x: x[0])
    for test, counts in by_base:
        clone_info = f" + {counts['clone']} clones" if counts['clone'] > 0 else ""
        print(f"  {test}: {counts['base']} base{clone_info}")


def main():
    if len(sys.argv) < 2:
        print(f"Usage: {sys.argv[0]} <artifacts_dir>")
        print()
        print("Download artifacts first:")
        print("  gh run download <run_id> --dir /tmp/ci-artifacts")
        print("  gh run view <run_id> --log > /tmp/ci-artifacts/full.log")
        sys.exit(1)
    
    artifacts_dir = Path(sys.argv[1])
    if not artifacts_dir.exists():
        print(f"Error: {artifacts_dir} does not exist")
        sys.exit(1)
    
    vm_results = analyze_logs(str(artifacts_dir))
    
    # Try to find full.log
    full_log = artifacts_dir / "full.log"
    if not full_log.exists():
        full_log = artifacts_dir.parent / "full.log"
    
    log_results = None
    if full_log.exists():
        log_results = analyze_full_log(full_log)
    
    print_report(vm_results, log_results)


if __name__ == '__main__':
    main()
