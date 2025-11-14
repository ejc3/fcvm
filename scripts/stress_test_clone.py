#!/usr/bin/env python3 -u
"""
Stress test for fcvm snapshot/clone performance.

Measures:
- Clone startup time (from command start to "VM cloned successfully")
- Time to first nginx response (health check)
- Success rate (percentage of VMs that pass health check)

This script manages the full lifecycle:
- Kills any existing VMs/servers
- Starts the UFFD memory server
- Clones VMs and measures performance
- Cleans up all processes

Usage:
    python3 -u scripts/stress_test_clone.py --snapshot final --num-clones 10 -v
"""

import asyncio
import argparse
import time
import sys
import signal
from pathlib import Path
from dataclasses import dataclass
from typing import List, Optional
import re

# Disable output buffering
sys.stdout.reconfigure(line_buffering=True)
sys.stderr.reconfigure(line_buffering=True)


@dataclass
class CloneMetrics:
    """Metrics for a single clone VM."""
    name: str
    clone_start_time: float
    clone_end_time: float
    first_response_time: Optional[float] = None
    tap_device: Optional[str] = None
    error: Optional[str] = None

    @property
    def clone_duration(self) -> float:
        """Time to clone VM (seconds)."""
        return self.clone_end_time - self.clone_start_time

    @property
    def time_to_first_response(self) -> Optional[float]:
        """Time from clone start to first nginx response (seconds)."""
        if self.first_response_time:
            return self.first_response_time - self.clone_start_time
        return None


async def run_command(cmd: List[str], timeout: int = 30) -> tuple[str, str, int]:
    """Run a command and return (stdout, stderr, returncode)."""
    proc = await asyncio.create_subprocess_exec(
        *cmd,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
    )

    try:
        stdout, stderr = await asyncio.wait_for(proc.communicate(), timeout=timeout)
        return stdout.decode(), stderr.decode(), proc.returncode
    except asyncio.TimeoutError:
        proc.kill()
        await proc.wait()
        raise


async def start_memory_server(snapshot: str, fcvm_path: Path, verbose: bool = False):
    """Start the UFFD memory server for the snapshot. Returns the server process."""
    if verbose:
        print(f"Starting memory server for snapshot '{snapshot}'...", flush=True)

    cmd = [
        "sudo",
        str(fcvm_path),
        "snapshot",
        "serve",
        snapshot,
    ]

    proc = await asyncio.create_subprocess_exec(
        *cmd,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.STDOUT,
    )

    # Wait for server to be ready (look for "UFFD server listening")
    ready_marker = b"UFFD server listening"
    try:
        while True:
            line = await asyncio.wait_for(proc.stdout.readline(), timeout=10)
            if not line:
                raise RuntimeError("Memory server terminated unexpectedly")

            if verbose:
                print(f"  [server] {line.decode().strip()}", flush=True)

            if ready_marker in line:
                if verbose:
                    print(f"✓ Memory server ready", flush=True)
                return proc

    except asyncio.TimeoutError:
        proc.kill()
        await proc.wait()
        raise RuntimeError("Memory server failed to start within 10 seconds")


async def clone_vm(snapshot: str, clone_name: str, fcvm_path: Path) -> CloneMetrics:
    """Clone a VM and measure startup time."""
    metrics = CloneMetrics(
        name=clone_name,
        clone_start_time=time.time(),
        clone_end_time=0,
    )

    cmd = [
        "sudo",
        str(fcvm_path),
        "snapshot",
        "run",
        snapshot,
        "--name",
        clone_name,
        "--mode",
        "rootless",
    ]

    # Start clone in background (it runs forever)
    proc = await asyncio.create_subprocess_exec(
        *cmd,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.STDOUT,
    )

    # Read output until we see "VM cloned successfully"
    success_marker = b"VM cloned successfully"
    tap_pattern = re.compile(rb'network configured for clone.*?(tap-[a-z0-9-]+)')

    try:
        timeout = 5  # Initial timeout for startup
        while True:
            line = await asyncio.wait_for(proc.stdout.readline(), timeout=timeout)
            if not line:
                break

            if success_marker in line:
                metrics.clone_end_time = time.time()
                # After success, use shorter timeout since output slows down
                timeout = 2

            # Extract TAP device name
            match = tap_pattern.search(line)
            if match:
                metrics.tap_device = match.group(1).decode()

            # If we have both metrics, we're done reading
            if metrics.clone_end_time > 0 and metrics.tap_device:
                # Leave VM running for health check
                break

    except asyncio.TimeoutError:
        # If we timed out but have the metrics, it's OK
        if metrics.clone_end_time > 0 and metrics.tap_device:
            pass  # Leave VM running for health check
        else:
            metrics.error = "Timeout waiting for VM to start"
            proc.kill()
            await proc.wait()
            return metrics

    if metrics.clone_end_time == 0:
        metrics.error = "Never saw 'VM cloned successfully' message"
        metrics.clone_end_time = time.time()
        proc.kill()
        await proc.wait()
        return metrics

    # Leave VM process running for health check (don't wait for it)
    return metrics


async def test_nginx(tap_device: str, guest_ip: str = "172.16.0.174", timeout: int = 5) -> tuple[bool, float]:
    """Test nginx response via TAP device. Returns (success, duration)."""
    start = time.time()

    cmd = [
        "curl",
        "-s",
        "-m", str(timeout),
        "--interface", tap_device,
        f"http://{guest_ip}",
    ]

    try:
        stdout, stderr, returncode = await run_command(cmd, timeout=timeout + 1)
        duration = time.time() - start

        if returncode == 0 and "nginx" in stdout.lower():
            return True, duration
        return False, duration

    except Exception as e:
        return False, time.time() - start


async def wait_for_nginx(tap_device: str, max_attempts: int = 30, delay: float = 1.0, verbose: bool = False) -> Optional[float]:
    """Wait for nginx to respond. Returns response time or None if timeout."""
    for attempt in range(max_attempts):
        if verbose and attempt > 0:
            print(f"    Attempt {attempt + 1}/{max_attempts} for {tap_device}...", flush=True)
        success, duration = await test_nginx(tap_device)
        if success:
            return time.time()
        await asyncio.sleep(delay)
    return None


async def stress_test(
    snapshot: str,
    num_clones: int,
    fcvm_path: Path,
    batch_size: int = 5,
    verbose: bool = False,
) -> List[CloneMetrics]:
    """Run stress test: clone multiple VMs and measure performance."""

    print(f"Starting stress test: {num_clones} clones from snapshot '{snapshot}'")
    print(f"Batch size: {batch_size} concurrent clones")
    print("-" * 80)

    all_metrics: List[CloneMetrics] = []

    # Clone in batches to avoid overwhelming the system
    for batch_start in range(0, num_clones, batch_size):
        batch_end = min(batch_start + batch_size, num_clones)
        batch_num = (batch_start // batch_size) + 1

        print(f"\nBatch {batch_num}: Cloning VMs {batch_start + 1}-{batch_end}...", flush=True)

        # Start clones concurrently
        if verbose:
            print(f"  Starting {batch_end - batch_start} clone tasks...", flush=True)

        clone_tasks = [
            clone_vm(snapshot, f"clone{i}", fcvm_path)
            for i in range(batch_start, batch_end)
        ]

        batch_metrics = await asyncio.gather(*clone_tasks)

        # Print clone results
        for metrics in batch_metrics:
            if metrics.error:
                print(f"  ✗ {metrics.name}: {metrics.error}")
            else:
                print(f"  ✓ {metrics.name}: cloned in {metrics.clone_duration:.3f}s (TAP: {metrics.tap_device})")

        # Wait for nginx to respond on each VM (health check)
        print(f"  Waiting for nginx health checks...", flush=True)
        nginx_tasks = [
            wait_for_nginx(m.tap_device, verbose=verbose) if m.tap_device else asyncio.sleep(0)
            for m in batch_metrics
        ]

        nginx_times = await asyncio.gather(*nginx_tasks)

        for metrics, nginx_time in zip(batch_metrics, nginx_times):
            metrics.first_response_time = nginx_time
            if nginx_time:
                ttfr = metrics.time_to_first_response
                print(f"  ✓ {metrics.name}: nginx healthy in {ttfr:.3f}s from clone start", flush=True)
            else:
                print(f"  ✗ {metrics.name}: nginx health check timeout", flush=True)

        all_metrics.extend(batch_metrics)

        # Small delay between batches
        if batch_end < num_clones:
            await asyncio.sleep(2)

    return all_metrics


def print_summary(metrics: List[CloneMetrics]):
    """Print summary statistics."""
    print("\n" + "=" * 80)
    print("SUMMARY")
    print("=" * 80)

    successful = [m for m in metrics if not m.error]
    failed = [m for m in metrics if m.error]

    print(f"\nTotal VMs: {len(metrics)}")
    print(f"  Successful: {len(successful)}")
    print(f"  Failed: {len(failed)}")

    if not successful:
        print("\nNo successful clones to analyze.")
        return

    clone_times = [m.clone_duration for m in successful]
    print(f"\nClone Time (seconds):")
    print(f"  Min: {min(clone_times):.3f}s")
    print(f"  Max: {max(clone_times):.3f}s")
    print(f"  Avg: {sum(clone_times) / len(clone_times):.3f}s")

    nginx_responsive = [m for m in successful if m.first_response_time]
    if nginx_responsive:
        ttfr_times = [m.time_to_first_response for m in nginx_responsive]
        print(f"\nTime to First Response (seconds):")
        print(f"  Min: {min(ttfr_times):.3f}s")
        print(f"  Max: {max(ttfr_times):.3f}s")
        print(f"  Avg: {sum(ttfr_times) / len(ttfr_times):.3f}s")
        print(f"  Success rate: {len(nginx_responsive)}/{len(successful)} ({len(nginx_responsive)*100/len(successful):.1f}%)")

    if failed:
        print(f"\nFailed clones:")
        for m in failed:
            print(f"  {m.name}: {m.error}")


async def cleanup_all(verbose: bool = False):
    """Kill all VMs and memory servers."""
    if verbose:
        print("\nCleaning up all processes (VMs and memory servers)...", flush=True)

    # Kill everything: firecracker VMs, fcvm processes, and any lingering bash/python
    await run_command(["sudo", "killall", "-9", "firecracker", "fcvm"], timeout=5)
    await asyncio.sleep(2)

    if verbose:
        print("✓ Cleanup complete", flush=True)


async def main():
    parser = argparse.ArgumentParser(description="Stress test fcvm clone performance")
    parser.add_argument(
        "--snapshot",
        default="final",
        help="Snapshot name to clone from",
    )
    parser.add_argument(
        "--num-clones",
        type=int,
        default=10,
        help="Number of VMs to clone",
    )
    parser.add_argument(
        "--batch-size",
        type=int,
        default=5,
        help="Number of concurrent clones per batch",
    )
    parser.add_argument(
        "--fcvm-path",
        type=Path,
        default=Path("target/release/fcvm"),
        help="Path to fcvm binary",
    )
    parser.add_argument(
        "--no-cleanup",
        action="store_true",
        help="Don't kill VMs at the end",
    )
    parser.add_argument(
        "-v", "--verbose",
        action="store_true",
        help="Enable verbose output",
    )

    args = parser.parse_args()

    if not args.fcvm_path.exists():
        print(f"Error: fcvm binary not found at {args.fcvm_path}", file=sys.stderr)
        sys.exit(1)

    memory_server = None

    try:
        # Clean up any existing processes first
        print("Cleaning up any existing VMs and servers...", flush=True)
        await cleanup_all(verbose=args.verbose)

        # Start memory server
        memory_server = await start_memory_server(
            snapshot=args.snapshot,
            fcvm_path=args.fcvm_path,
            verbose=args.verbose,
        )

        # Give server a moment to fully initialize
        await asyncio.sleep(1)

        # Run stress test
        metrics = await stress_test(
            snapshot=args.snapshot,
            num_clones=args.num_clones,
            fcvm_path=args.fcvm_path,
            batch_size=args.batch_size,
            verbose=args.verbose,
        )

        # Print results
        print_summary(metrics)

    finally:
        if not args.no_cleanup:
            # Kill memory server first
            if memory_server:
                if args.verbose:
                    print("\nStopping memory server...", flush=True)
                memory_server.terminate()
                try:
                    await asyncio.wait_for(memory_server.wait(), timeout=5)
                except asyncio.TimeoutError:
                    memory_server.kill()
                    await memory_server.wait()

            # Then cleanup all VMs
            await cleanup_all(verbose=args.verbose)


if __name__ == "__main__":
    asyncio.run(main())
