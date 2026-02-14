//! Hugepages benchmark: measures clone restore speed for large dirty-memory VMs.
//!
//! Compares standard 4KB pages vs 2MB hugepages for snapshot/clone operations.
//! Exploits the existing `fcvm podman run` snapshot cache flow:
//!   1. First run (cold): imports container image → dirties VM page cache → creates snapshots
//!   2. Second run (warm): restores from startup snapshot (this is the clone)
//!
//! The container image contains random data whose size controls how much VM memory
//! gets dirtied during import (page cache fills as fc-agent reads the tar archive).
//!
//! Full mode:  32GB VM, 16GB dirty memory (~20 min)
//! Test mode:  2GB VM, 256MB dirty memory (~5 min)
//!
//! Run with: make bench-hugepages       (full)
//!           make bench-hugepages-test  (test)

use serde::Deserialize;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BENCH_IMAGE: &str = "localhost/bench-hugepages";

/// VM state from fcvm ls --json
#[derive(Deserialize)]
struct VmLsEntry {
    health_status: String,
}

/// Snapshot info from fcvm snapshots ls --json
#[derive(Deserialize)]
struct SnapshotLsEntry {
    name: String,
    memory_size_bytes: u64,
}

struct BenchResult {
    first_run_secs: f64,
    snapshot_size_mb: u64,
    clone_secs: f64,
}

/// Find the fcvm binary
fn find_fcvm_binary() -> PathBuf {
    let candidates = [
        "./target/release/fcvm",
        "./target/debug/fcvm",
        "/usr/local/bin/fcvm",
    ];
    for path in candidates {
        let p = PathBuf::from(path);
        if p.exists() {
            return p;
        }
    }
    panic!("fcvm binary not found - run: cargo build --release");
}

/// Kill a process gracefully (SIGTERM, then SIGKILL after timeout)
fn graceful_kill(pid: u32, timeout_ms: u64) {
    let _ = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .output();

    let start = Instant::now();
    loop {
        let status = Command::new("kill").args(["-0", &pid.to_string()]).output();
        match status {
            Ok(o) if !o.status.success() => return,
            _ => {}
        }
        if start.elapsed() > Duration::from_millis(timeout_ms) {
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).output();
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Check if a process is still alive
fn is_process_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Wait for a VM to become healthy, polling fcvm ls --json --pid.
/// Detects early process death to avoid waiting the full timeout.
fn poll_health(fcvm: &Path, pid: u32, timeout_secs: u64) -> Duration {
    let start = Instant::now();
    let timeout = Duration::from_secs(timeout_secs);
    loop {
        if start.elapsed() > timeout {
            panic!(
                "VM (PID {}) failed to become healthy within {}s",
                pid, timeout_secs
            );
        }

        // Detect early process death
        if !is_process_alive(pid) {
            panic!(
                "VM process (PID {}) died after {:.1}s — check /tmp/fcvm-bench-*.log",
                pid,
                start.elapsed().as_secs_f64()
            );
        }

        let output = Command::new(fcvm)
            .args(["ls", "--json", "--pid", &pid.to_string()])
            .output()
            .expect("failed to run fcvm ls");

        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if let Ok(vms) = serde_json::from_str::<Vec<VmLsEntry>>(&stdout) {
                if vms
                    .first()
                    .map(|v| v.health_status == "healthy")
                    .unwrap_or(false)
                {
                    return start.elapsed();
                }
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Delete benchmark snapshots via: fcvm snapshots prune --all --image <BENCH_IMAGE> --force
fn prune_bench_snapshots(fcvm: &Path) {
    let output = Command::new(fcvm)
        .args([
            "snapshots",
            "prune",
            "--all",
            "--image",
            BENCH_IMAGE,
            "--force",
        ])
        .output()
        .expect("failed to run fcvm snapshots prune");

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            eprintln!("    {}", line);
        }
    }
}

/// Poll fcvm snapshots ls --json --image <BENCH_IMAGE> for a startup snapshot
fn wait_for_startup_snapshot(fcvm: &Path, timeout_secs: u64) -> Option<SnapshotLsEntry> {
    let start = Instant::now();
    loop {
        let output = Command::new(fcvm)
            .args(["snapshots", "ls", "--json", "--image", BENCH_IMAGE])
            .output()
            .expect("failed to run fcvm snapshots ls");

        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if let Ok(snapshots) = serde_json::from_str::<Vec<SnapshotLsEntry>>(&stdout) {
                if let Some(startup) = snapshots.into_iter().find(|s| s.name.ends_with("-startup"))
                {
                    return Some(startup);
                }
            }
        }

        if start.elapsed() > Duration::from_secs(timeout_secs) {
            return None;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// Build the benchmark container image with the specified data size
fn build_image(data_mb: u32) {
    eprintln!(
        "==> Building benchmark container image ({} MB data)...",
        data_mb
    );

    // Check if image already exists with correct data size
    let inspect = Command::new("podman")
        .args([
            "inspect",
            BENCH_IMAGE,
            "--format",
            "{{.Config.Labels.data_size_mb}}",
        ])
        .output();
    if let Ok(output) = inspect {
        if output.status.success() {
            let label = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if label == data_mb.to_string() {
                eprintln!("    Image already exists with correct data size, skipping build");
                return;
            }
        }
    }

    let status = Command::new("podman")
        .args([
            "build",
            "-t",
            BENCH_IMAGE,
            "-f",
            "Containerfile.bench-hugepages",
            "--build-arg",
            &format!("DATA_SIZE_MB={}", data_mb),
            "--label",
            &format!("data_size_mb={}", data_mb),
            ".",
        ])
        .status()
        .expect("failed to run podman build — is podman installed?");

    if !status.success() {
        panic!("podman build failed");
    }
    eprintln!("    Image built successfully");
}

/// RAII guard that kills a process on drop
struct ProcessGuard {
    pid: u32,
    child: Child,
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        graceful_kill(self.pid, 5000);
        let _ = self.child.wait();
    }
}

/// Run one mode (standard or hugepages) through the full benchmark cycle
fn run_mode(mode: &str, mem_mb: u32, data_mb: u32, hugepages: bool) -> BenchResult {
    let fcvm = find_fcvm_binary();
    let pid_suffix = std::process::id();
    let mem_str = mem_mb.to_string();

    eprintln!("\n--- {} mode ---", mode);

    // Delete only benchmark snapshots to ensure cold start
    prune_bench_snapshots(&fcvm);

    // First run: cold start (cache miss)
    let name1 = format!("bench-hp-{}-cold-{}", mode, pid_suffix);
    eprintln!("  [1/2] Cold start: {}", name1);

    let log_path1 = format!("/tmp/fcvm-bench-{}.log", name1);
    let log_file1 = File::create(&log_path1).expect("create log file");
    let log_err1 = log_file1.try_clone().expect("clone log file");

    let mut args1 = vec![
        "podman",
        "run",
        "--name",
        &name1,
        "--mem",
        &mem_str,
        "--health-check",
        "http://localhost:80/",
    ];
    if hugepages {
        args1.push("--hugepages");
    }
    args1.push(BENCH_IMAGE);

    let t1 = Instant::now();
    let child1 = Command::new(&fcvm)
        .args(&args1)
        .env("RUST_LOG", "debug")
        .stdout(Stdio::from(log_file1))
        .stderr(Stdio::from(log_err1))
        .spawn()
        .expect("failed to spawn cold start VM");

    let pid1 = child1.id();
    let _guard1 = ProcessGuard {
        pid: pid1,
        child: child1,
    };
    eprintln!("    Log: {}", log_path1);

    let healthy_timeout = if data_mb > 1000 { 900 } else { 300 };
    let health_elapsed = poll_health(&fcvm, pid1, healthy_timeout);
    eprintln!("    Healthy after {:.1}s", health_elapsed.as_secs_f64());

    // Wait for startup snapshot
    eprintln!("    Waiting for startup snapshot...");
    let snap_wait = Instant::now();
    let snapshot_size_mb = match wait_for_startup_snapshot(&fcvm, 120) {
        Some(snap) => {
            eprintln!(
                "    Startup snapshot '{}' created after {:.1}s",
                snap.name,
                snap_wait.elapsed().as_secs_f64()
            );
            snap.memory_size_bytes / (1024 * 1024)
        }
        None => {
            eprintln!("    WARNING: startup snapshot not detected after 120s");
            0
        }
    };
    eprintln!("    Memory snapshot size: {} MB", snapshot_size_mb);

    let first_run_secs = t1.elapsed().as_secs_f64();

    eprintln!("    Killing cold start VM...");
    drop(_guard1);

    // Wait for hugepages to be freed (Firecracker under unshare may take a moment)
    if hugepages {
        let needed = (mem_mb as u64) / 2;
        let start_wait = Instant::now();
        loop {
            let free =
                std::fs::read_to_string("/sys/kernel/mm/hugepages/hugepages-2048kB/free_hugepages")
                    .unwrap_or_default()
                    .trim()
                    .parse::<u64>()
                    .unwrap_or(0);
            if free >= needed {
                break;
            }
            if start_wait.elapsed() > Duration::from_secs(30) {
                panic!(
                    "Hugepages not freed after 30s: need {} but only {} free",
                    needed, free
                );
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    } else {
        std::thread::sleep(Duration::from_secs(2));
    }

    // Second run: warm start (cache hit → clone from startup snapshot)
    let name2 = format!("bench-hp-{}-warm-{}", mode, pid_suffix);
    eprintln!("  [2/2] Warm start (clone): {}", name2);

    let log_path2 = format!("/tmp/fcvm-bench-{}.log", name2);
    let log_file2 = File::create(&log_path2).expect("create log file");
    let log_err2 = log_file2.try_clone().expect("clone log file");

    let mut args2 = vec![
        "podman",
        "run",
        "--name",
        &name2,
        "--mem",
        &mem_str,
        "--health-check",
        "http://localhost:80/",
    ];
    if hugepages {
        args2.push("--hugepages");
    }
    args2.push(BENCH_IMAGE);

    let t2 = Instant::now();
    let child2 = Command::new(&fcvm)
        .args(&args2)
        .env("RUST_LOG", "debug")
        .stdout(Stdio::from(log_file2))
        .stderr(Stdio::from(log_err2))
        .spawn()
        .expect("failed to spawn warm start VM");

    let pid2 = child2.id();
    let _guard2 = ProcessGuard {
        pid: pid2,
        child: child2,
    };
    eprintln!("    Log: {}", log_path2);

    let clone_timeout = if data_mb > 1000 { 300 } else { 120 };
    let clone_elapsed = poll_health(&fcvm, pid2, clone_timeout);
    let clone_secs = clone_elapsed.as_secs_f64();
    eprintln!("    Clone healthy after {:.1}s", clone_secs);

    let total_clone_secs = t2.elapsed().as_secs_f64();

    eprintln!("    Killing clone VM...");
    drop(_guard2);

    BenchResult {
        first_run_secs,
        snapshot_size_mb,
        clone_secs: total_clone_secs,
    }
}

fn print_comparison(mem_mb: u32, data_mb: u32, std: &BenchResult, hp: &BenchResult) {
    eprintln!();
    println!("=================================================================");
    if data_mb >= 1024 {
        println!(
            "  Hugepages Benchmark: {}GB VM, {}GB dirty memory",
            mem_mb / 1024,
            data_mb / 1024
        );
    } else {
        println!(
            "  Hugepages Benchmark: {}GB VM, {}MB dirty memory",
            mem_mb / 1024,
            data_mb
        );
    }
    println!("=================================================================");
    println!();
    println!(
        "{:<24} {:>17} {:>17} {:>8}",
        "Phase", "Standard (4KB)", "Hugepages (2MB)", "Ratio"
    );
    println!("{}", "-".repeat(68));
    println!(
        "{:<24} {:>16.1}s {:>16.1}s {:>7.2}x",
        "First Run (cold)",
        std.first_run_secs,
        hp.first_run_secs,
        hp.first_run_secs / std.first_run_secs.max(0.001)
    );
    println!(
        "{:<24} {:>14} MB {:>14} MB {:>7.2}x",
        "Snapshot Size",
        std.snapshot_size_mb,
        hp.snapshot_size_mb,
        hp.snapshot_size_mb as f64 / std.snapshot_size_mb.max(1) as f64
    );
    println!(
        "{:<24} {:>16.1}s {:>16.1}s {:>7.2}x",
        "Clone Restore",
        std.clone_secs,
        hp.clone_secs,
        hp.clone_secs / std.clone_secs.max(0.001)
    );
    println!("{}", "-".repeat(68));
    println!();
}

fn main() {
    let fcvm = find_fcvm_binary();
    let args: Vec<String> = std::env::args().collect();
    let test_mode = args.iter().any(|a| a == "--test");

    let (mem_mb, data_mb): (u32, u32) = if test_mode {
        eprintln!("=== Hugepages Benchmark (TEST MODE: 2GB VM, 256MB data) ===");
        (2048, 256)
    } else {
        eprintln!("=== Hugepages Benchmark (FULL: 32GB VM, 16GB data) ===");
        (32768, 16384)
    };

    // Build container image (skips if already exists with correct size)
    build_image(data_mb);

    // Run standard mode
    let std_result = run_mode("standard", mem_mb, data_mb, false);

    // Run hugepages mode
    let hp_result = run_mode("hugepages", mem_mb, data_mb, true);

    // Print comparison
    print_comparison(mem_mb, data_mb, &std_result, &hp_result);

    // Clean up benchmark snapshots
    eprintln!("==> Cleaning up benchmark snapshots...");
    prune_bench_snapshots(&fcvm);
}
