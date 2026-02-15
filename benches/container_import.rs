//! Container import benchmark: compares legacy `podman load` vs direct image mount.
//!
//! Measures cold start time for localhost/ container images using two strategies:
//!   - Legacy: Docker archive attached as block device, fc-agent runs `podman load`
//!   - Direct mount: Pre-built podman storage mounted as additionalImageStore
//!
//! Both modes are tested without snapshot cache (--no-snapshot) to isolate the
//! import step from snapshot restore performance.
//!
//! Run with: make bench-container-import

use serde::Deserialize;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BENCH_IMAGE: &str = "localhost/bench-container-import";

const BENCH_DOCKERFILE: &[u8] = b"FROM public.ecr.aws/nginx/nginx:alpine\n\
    RUN dd if=/dev/urandom of=/data1.bin bs=1M count=100 2>/dev/null\n\
    RUN dd if=/dev/urandom of=/data2.bin bs=1M count=100 2>/dev/null\n\
    RUN dd if=/dev/urandom of=/data3.bin bs=1M count=100 2>/dev/null\n\
    RUN dd if=/dev/urandom of=/data4.bin bs=1M count=100 2>/dev/null\n\
    RUN dd if=/dev/urandom of=/data5.bin bs=1M count=100 2>/dev/null\n\
    CMD [\"nginx\", \"-g\", \"daemon off;\"]\n";

/// VM state from fcvm ls --json
#[derive(Deserialize)]
struct VmLsEntry {
    health_status: String,
}

struct BenchResult {
    cold_start_secs: f64,
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
        let status = Command::new("kill")
            .args(["-0", &pid.to_string()])
            .output();
        match status {
            Ok(o) if !o.status.success() => return,
            _ => {}
        }
        if start.elapsed() > Duration::from_millis(timeout_ms) {
            let _ = Command::new("kill")
                .args(["-9", &pid.to_string()])
                .output();
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Wait for a VM to become healthy, polling fcvm ls --json --pid.
/// Uses try_wait() on the Child to detect early exit (avoids zombie false-positive
/// from kill -0).
fn poll_health(fcvm: &Path, child: &mut Child, timeout_secs: u64) -> Duration {
    let pid = child.id();
    let start = Instant::now();
    let timeout = Duration::from_secs(timeout_secs);
    loop {
        if start.elapsed() > timeout {
            panic!(
                "VM (PID {}) failed to become healthy within {}s",
                pid, timeout_secs
            );
        }

        // Check if the child process has exited
        match child.try_wait() {
            Ok(Some(status)) => {
                panic!(
                    "VM process (PID {}) exited with {} after {:.1}s - check /tmp/fcvm-bench-*.log",
                    pid,
                    status,
                    start.elapsed().as_secs_f64()
                );
            }
            Ok(None) => {} // still running
            Err(e) => {
                panic!("Failed to check VM process (PID {}): {}", pid, e);
            }
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

/// Delete cached image artifacts so the next run is a true cold start.
/// Removes both .storage.img (direct mount) and .docker.tar (legacy) to ensure
/// symmetric benchmarking — both modes pay the full export + build cost.
fn clear_image_cache() {
    eprintln!("    Clearing image cache...");
    let _ = Command::new("sh")
        .args([
            "-c",
            "rm -f /mnt/fcvm-btrfs/image-cache/*.storage.img /mnt/fcvm-btrfs/image-cache/*.docker.tar",
        ])
        .output();
}

/// Delete benchmark snapshots
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

/// Build the benchmark container image. Always runs podman build and relies on
/// podman's layer cache — a no-op rebuild if the Dockerfile hasn't changed.
fn build_image() {
    eprintln!("==> Building benchmark container image...");

    let status = Command::new("podman")
        .args(["build", "-t", BENCH_IMAGE, "-f", "-", "."])
        .stdin(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(ref mut stdin) = child.stdin {
                stdin.write_all(BENCH_DOCKERFILE)?;
            }
            child.wait()
        })
        .expect("failed to run podman build");

    if !status.success() {
        panic!("podman build failed");
    }
    eprintln!("    Image built successfully");
}

/// Run one mode through a cold start cycle (no snapshot cache)
fn run_mode(mode: &str, no_direct_image_mount: bool, iterations: u32) -> BenchResult {
    let fcvm = find_fcvm_binary();
    let pid_suffix = std::process::id();

    eprintln!("\n--- {} ---", mode);

    let mut total_secs = 0.0;

    for i in 1..=iterations {
        // Clean state for each iteration
        prune_bench_snapshots(&fcvm);
        // Clear cached image artifacts so both modes measure full cold start
        clear_image_cache();

        let name = format!("bench-import-{}-{}-{}", mode, i, pid_suffix);
        eprintln!("  [{}/{}] Cold start: {}", i, iterations, name);

        let log_path = format!("/tmp/fcvm-bench-{}.log", name);
        let log_file = File::create(&log_path).expect("create log file");
        let log_err = log_file.try_clone().expect("clone log file");

        let mut args = vec![
            "podman",
            "run",
            "--name",
            &name,
            "--no-snapshot",
            "--health-check",
            "http://localhost:80/",
        ];
        if no_direct_image_mount {
            args.push("--no-direct-image-mount");
        }
        args.push(BENCH_IMAGE);

        let t = Instant::now();
        let child = Command::new(&fcvm)
            .args(&args)
            .env("RUST_LOG", "debug")
            .stdout(Stdio::from(log_file))
            .stderr(Stdio::from(log_err))
            .spawn()
            .expect("failed to spawn VM");

        let pid = child.id();
        let mut guard = ProcessGuard { pid, child };
        eprintln!("    Log: {}", log_path);

        let health_elapsed = poll_health(&fcvm, &mut guard.child, 300);
        let elapsed = t.elapsed().as_secs_f64();
        eprintln!(
            "    Healthy after {:.1}s (total {:.1}s)",
            health_elapsed.as_secs_f64(),
            elapsed
        );

        total_secs += elapsed;

        eprintln!("    Killing VM...");
        drop(guard);
        std::thread::sleep(Duration::from_secs(2));
    }

    let cold_start_secs = total_secs / iterations as f64;
    eprintln!("  Average cold start: {:.1}s", cold_start_secs);

    BenchResult { cold_start_secs }
}

fn print_comparison(legacy: &BenchResult, direct: &BenchResult, iterations: u32) {
    eprintln!();
    println!("=================================================================");
    println!("  Container Import Benchmark ({} iterations each)", iterations);
    println!("=================================================================");
    println!();
    println!(
        "{:<24} {:>17} {:>17} {:>8}",
        "Metric", "podman load", "direct mount", "Speedup"
    );
    println!("{}", "-".repeat(68));
    println!(
        "{:<24} {:>16.1}s {:>16.1}s {:>7.2}x",
        "Cold Start (avg)",
        legacy.cold_start_secs,
        direct.cold_start_secs,
        legacy.cold_start_secs / direct.cold_start_secs.max(0.001)
    );
    println!("{}", "-".repeat(68));
    println!();
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let iterations: u32 = args
        .iter()
        .position(|a| a == "--iterations")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(3);

    eprintln!(
        "=== Container Import Benchmark ({} iterations) ===",
        iterations
    );

    // Build container image
    build_image();

    // Run legacy mode (podman load)
    let legacy_result = run_mode("podman-load", true, iterations);

    // Run direct mount mode
    let direct_result = run_mode("direct-mount", false, iterations);

    // Print comparison
    print_comparison(&legacy_result, &direct_result, iterations);

    // Clean up
    eprintln!("==> Cleaning up...");
    let fcvm = find_fcvm_binary();
    prune_bench_snapshots(&fcvm);
}
